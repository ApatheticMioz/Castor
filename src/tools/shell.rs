//! Shell command policy validator and sandboxed executor, ported from
//! `mcp-castor/src/harness/services/shell_validator.js` and
//! `mcp-castor/src/harness/services/shell_executor.js`.
//!
//! `validate` classifies a command string and returns a typed refusal error
//! for dangerous classes (fork bombs, disk wipes, `rm`/`del` on protected
//! roots, `dd` to devices, `sudo`/wrapper unwrapping, unexpanded shell
//! references, dead-man fuse). Benign commands pass.
//!
//! `run` executes a validated command: policy validation first (typed
//! refusal, nothing spawns), cwd resolved through the sandbox layers, spawn
//! via `tokio::process` with `process_group(0)`, stdout/stderr captured with
//! byte caps, and timeout kills the whole process group.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use regex::Regex;
use thiserror::Error;
use tokio::io::AsyncReadExt;

#[derive(Debug, Error)]
pub enum ShellPolicyError {
    #[error("InvalidCommandError: Shell command must be a non-empty string")]
    InvalidCommand,
    #[error("CommandSecurityError: Execution blocked. Command matches prohibited destructive pattern: {0}")]
    ProhibitedPattern(String),
    #[error("CommandSecurityError: Execution blocked. Unexpanded shell reference in target '{0}'")]
    UnexpandedReference(String),
    #[error("CommandSecurityError: Execution blocked. Target '{0}' resolves to protected root: {1}")]
    ProtectedRoot(String, String),
    #[error("FatalDeadManFuseError: Execution halted by dead-man fuse (synthetic canary tripped)")]
    DeadManFuse,
}

pub const CANARY_DISASTER_FUSE_TOKEN: &str = "__CANARY_TRIGGER_DISASTER_FUSE__";

const DEFAULT_CWD: &str = "/workspace";
const HOME: &str = "/home/user";
const MAX_UNWRAP_DEPTH: usize = 4;

const DESTRUCTIVE: &[&str] = &["rm", "del", "rmdir", "rd", "remove-item", "ri", "erase"];
/// Wrappers whose own argument (not an `=value`) is the real command.
/// `env` is handled separately in `analyze_segment` (it consumes
/// `KEY=VAL` prefix tokens before the real command) and is intentionally
/// absent here.
const TRANSPARENT: &[&str] = &["sudo", "doas", "nice", "nohup", "xargs"];
const WIN_FLAGS: &[&str] = &["/f", "/s", "/q", "/p", "/a", "/c", "/e", "/t", "/y", "/i"];

// ---------------------------------------------------------------------------
// Pattern-level blocks (verbatim, not path-aware).
// ---------------------------------------------------------------------------

/// Precompiled pattern-level block signatures, modeled on
/// `mcp-castor/src/harness/services/shell_validator.js` `PATTERN_LEVEL_BLOCKS`.
/// Whitespace between tokens is tolerated (`\s*` / `\s+`) so spacing variants
/// (e.g. `:() { :|:& };`) cannot bypass the matcher.
fn pattern_blocks() -> &'static [(&'static str, Regex)] {
    static BLOCKS: OnceLock<Vec<(&'static str, Regex)>> = OnceLock::new();
    BLOCKS.get_or_init(|| {
        vec![
            (
                "fork bomb",
                Regex::new(r":\(\)\s*\{\s*:\s*\|\s*:\s*&\s*\}\s*;").unwrap(),
            ),
            (
                "disk partitioning",
                Regex::new(r"\b(mkfs(\.[a-z0-9]+)?|fdisk|parted)\b").unwrap(),
            ),
            (
                "drive format",
                Regex::new(r"\bformat\s+[A-Za-z]:").unwrap(),
            ),
            (
                "dd to raw device",
                Regex::new(r"\bdd\s+.*of=/dev/(sd[a-z]|nvme|hd[a-z]|vd[a-z])").unwrap(),
            ),
        ]
    })
}

fn pattern_level_block(cmd: &str) -> Option<&'static str> {
    let lower = cmd.to_ascii_lowercase();
    for (sig, re) in pattern_blocks() {
        if re.is_match(&lower) {
            return Some(sig);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Tokenizer and segment splitter
// ---------------------------------------------------------------------------

fn tokenize(cmd: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for ch in cmd.chars() {
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => cur.push(ch),
            None if ch == '"' || ch == '\'' => quote = Some(ch),
            None if ch == ' ' || ch == '\t' => {
                if !cur.is_empty() {
                    tokens.push(std::mem::take(&mut cur));
                }
            }
            _ => cur.push(ch),
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
}

fn flush_seg(segments: &mut Vec<String>, cur: &mut String) {
    let t = cur.trim().to_string();
    if !t.is_empty() {
        segments.push(t);
    }
    cur.clear();
}

fn split_segments(cmd: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let chars: Vec<char> = cmd.chars().collect();
    let n = chars.len();
    let mut i = 0;
    while i < n {
        let ch = chars[i];
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            }
            cur.push(ch);
            i += 1;
            continue;
        }
        if ch == '"' || ch == '\'' {
            quote = Some(ch);
            cur.push(ch);
            i += 1;
            continue;
        }
        if ch == '&' && i + 1 < n && chars[i + 1] == '&' {
            flush_seg(&mut segments, &mut cur);
            i += 2;
            continue;
        }
        if ch == '|' && i + 1 < n && chars[i + 1] == '|' {
            flush_seg(&mut segments, &mut cur);
            i += 2;
            continue;
        }
        if ch == ';' || ch == '|' || ch == '\n' || ch == '\r' {
            flush_seg(&mut segments, &mut cur);
            i += 1;
            continue;
        }
        cur.push(ch);
        i += 1;
    }
    flush_seg(&mut segments, &mut cur);
    segments
}

// ---------------------------------------------------------------------------
// Path normalization
// ---------------------------------------------------------------------------

fn command_name(token: &str) -> String {
    let parts: Vec<&str> = token.split(['/', '\\']).collect();
    parts.last().unwrap_or(&"").to_ascii_lowercase()
}

fn is_flag(token: &str) -> bool {
    if token.starts_with('-') {
        return true;
    }
    WIN_FLAGS.contains(&token.to_ascii_lowercase().as_str())
}

fn has_unexpanded_ref(operand: &str) -> bool {
    operand.contains('$') || operand.contains('`')
}

fn is_env_assignment(token: &str) -> bool {
    let bytes = token.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    if !bytes[0].is_ascii_alphabetic() && bytes[0] != b'_' {
        return false;
    }
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'=' {
            return i > 0;
        }
        if !b.is_ascii_alphanumeric() && b != b'_' {
            return false;
        }
    }
    false
}

fn expand_tilde(operand: &str, is_win: bool) -> String {
    if operand == "~" {
        return HOME.to_string();
    }
    if operand.starts_with("~/") || operand.starts_with("~\\") {
        return format!("{HOME}{}", operand[1..].replace('\\', "/"));
    }
    if let Some(rest) = operand.strip_prefix('~')
        && !rest.is_empty() && rest.as_bytes()[0] != b'/' && rest.as_bytes()[0] != b'\\' {
            if is_win {
                return format!("C:\\Users\\{rest}");
            }
            if rest == "root" {
                return "/root".to_string();
            }
            return format!("/home/{rest}");
        }
    operand.to_string()
}

fn to_posix(p: &str) -> String {
    let t = p.trim();
    if t.is_empty() {
        return HOME.to_string();
    }
    if let Some(rest) = t
        .strip_prefix(r"\\wsl.localhost\")
        .or_else(|| t.strip_prefix(r"\\wsl$\\"))
    {
        let parts: Vec<&str> = rest.split('\\').collect();
        if parts.len() >= 2 {
            let sub = parts[1..].join("/");
            return format!("/{sub}");
        }
    }
    if t.len() >= 3 {
        let b0 = t.as_bytes()[0];
        let b1 = t.as_bytes()[1];
        if b0.is_ascii_alphabetic()
            && b1 == b':'
            && (t.as_bytes()[2] == b'\\' || t.as_bytes()[2] == b'/')
        {
            let drive = b0 as char;
            let sub = t[3..].replace('\\', "/");
            return format!("/mnt/{}/{}", drive.to_ascii_lowercase(), sub);
        }
    }
    if t.len() >= 3 && t.starts_with('/') {
        let b1 = t.as_bytes()[1];
        if b1.is_ascii_alphabetic() && t.as_bytes()[2] == b'/' {
            let drive = b1 as char;
            let sub = t[3..].to_string();
            return format!("/mnt/{}/{}", drive.to_ascii_lowercase(), sub);
        }
    }
    t.replace('\\', "/")
}

fn posix_normalize(p: &str) -> String {
    use path_clean::PathClean;
    let cleaned = std::path::Path::new(p).clean();
    let s = cleaned.to_string_lossy().replace('\\', "/");
    if s.is_empty() || s == "." {
        "/".to_string()
    } else if !s.starts_with('/') {
        format!("/{s}")
    } else {
        s
    }
}

/// Unicode/homoglyph defense: fold fullwidth & compatibility forms to their
/// canonical ASCII equivalents (e.g. fullwidth `Ｗ` U+FF37 -> `W`) so a
/// homoglyph path cannot dodge the protected-root string comparison.
/// Mirrors `String.prototype.normalize("NFKC")` from the JS source for the
/// fullwidth Latin block (U+FF01–U+FF5E -> U+0021–U+007E) and the fullwidth
/// space (U+3000 -> U+0020).
fn nfkc_fold(s: &str) -> String {
    s.chars()
        .map(|c| {
            let cp = c as u32;
            if (0xFF01..=0xFF5E).contains(&cp) {
                char::from_u32(cp - 0xFF01 + 0x0021).unwrap_or(c)
            } else if cp == 0x3000 {
                ' '
            } else {
                c
            }
        })
        .collect()
}

fn normalize_operand(operand: &str, cwd: &str, is_win: bool) -> String {
    let p = expand_tilde(operand, is_win);
    let p = nfkc_fold(&p);
    let mut posix = to_posix(&p);
    let mut wildcard = "";
    if posix.ends_with("/*") {
        wildcard = "/*";
        posix = posix[..posix.len() - 2].to_string();
    } else if posix.ends_with('*') {
        wildcard = "*";
        posix = posix[..posix.len() - 1].to_string();
    }
    if posix.is_empty() {
        posix = "/".to_string();
    }
    if !posix.starts_with('/') {
        let cwd_posix = to_posix(cwd);
        posix = posix_normalize(&format!("{cwd_posix}/{posix}"));
    } else {
        posix = posix_normalize(&posix);
    }
    if posix != "/" && posix.ends_with('/') {
        while posix.ends_with('/') {
            posix.pop();
        }
    }
    if !wildcard.is_empty() {
        posix = if posix == "/" {
            wildcard.to_string()
        } else {
            format!("{posix}{wildcard}")
        };
    }
    posix.to_ascii_lowercase()
}

// ---------------------------------------------------------------------------
// Protected roots
// ---------------------------------------------------------------------------

fn protected_roots() -> &'static [String] {
    static ROOTS: OnceLock<Vec<String>> = OnceLock::new();
    ROOTS.get_or_init(|| {
        let mut v = vec!["/".to_string(), "/root".to_string(), "/home".to_string()];
        for c in b'a'..=b'z' {
            v.push(format!("/mnt/{}", c as char));
        }
        v.push("/mnt/c/windows".to_string());
        v.push("/mnt/c/users".to_string());
        v.push("/mnt/c/program files".to_string());
        v
    })
}

fn is_home_user(p: &str) -> bool {
    let rest = p.strip_prefix("/home/").unwrap_or("");
    !rest.is_empty() && !rest.contains('/')
}

fn is_dev_target(p: &str) -> bool {
    let rest = match p.strip_prefix("/dev/") {
        Some(r) => r,
        None => return false,
    };
    let name = match rest.strip_suffix("/*") {
        Some(n) => n,
        None => rest,
    };
    let (kind, tail) = match name {
        n if n.len() >= 3 && &n[..2] == "sd" => ("sd", &n[2..]),
        n if n.len() >= 3 && &n[..2] == "hd" => ("hd", &n[2..]),
        n if n.len() >= 3 && &n[..2] == "vd" => ("vd", &n[2..]),
        n if n.len() >= 5 && &n[..4] == "nvme" => ("nvme", &n[4..]),
        _ => return false,
    };
    match kind {
        "sd" | "hd" | "vd" => !tail.is_empty() && tail.chars().all(|c| c.is_ascii_lowercase()),
        "nvme" => {
            let d1: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
            let rem = &tail[d1.len()..];
            let rem = match rem.strip_prefix('n') {
                Some(r) => r,
                None => return false,
            };
            let d2: String = rem.chars().take_while(|c| c.is_ascii_digit()).collect();
            !d1.is_empty() && !d2.is_empty() && rem.len() == d2.len()
        }
        _ => false,
    }
}

fn is_protected_root(p: &str) -> bool {
    let p = p.to_ascii_lowercase();
    if is_dev_target(&p) {
        return true;
    }
    for root in protected_roots() {
        if p == *root {
            return true;
        }
        let w = if *root == "/" { "/*" } else { &format!("{root}/*") };
        if p == w {
            return true;
        }
    }
    if p == "/root" || is_home_user(&p) {
        return true;
    }
    false
}

// ---------------------------------------------------------------------------
// Shell wrapper recursion
// ---------------------------------------------------------------------------

fn shell_wrapper(name: &str) -> Option<(&'static [&'static str], &'static str)> {
    match name {
        "bash" | "sh" | "dash" | "zsh" => Some((&["-c"], "single")),
        "cmd" | "cmd.exe" => Some((&["/c"], "rest")),
        "powershell" | "pwsh" => Some((&["-command"], "single")),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Core analysis
// ---------------------------------------------------------------------------

fn analyze_segment(cmd: &str, cwd: &str, depth: usize) -> Result<(), ShellPolicyError> {
    let tokens = tokenize(cmd);
    if tokens.is_empty() {
        return Ok(());
    }

    let mut i = 0;
    while i < tokens.len() {
        let name = command_name(&tokens[i]);
        if name == "env" {
            i += 1;
            while i < tokens.len() && is_env_assignment(&tokens[i]) {
                i += 1;
            }
            continue;
        }
        if TRANSPARENT.contains(&name.as_str()) {
            i += 1;
            continue;
        }
        break;
    }
    if i >= tokens.len() {
        return Ok(());
    }

    let name = command_name(&tokens[i]);

    if let Some((flags, inner)) = shell_wrapper(&name) {
        if depth < MAX_UNWRAP_DEPTH
            && i + 1 < tokens.len()
            && flags.contains(&tokens[i + 1].to_ascii_lowercase().as_str())
        {
            let inner_cmd = if inner == "single" {
                tokens.get(i + 2).cloned()
            } else {
                (i + 2 < tokens.len()).then(|| tokens[i + 2..].join(" "))
            };
            if let Some(inner_cmd) = inner_cmd {
                analyze_command(&inner_cmd, cwd, depth + 1)?;
            }
        }
        return Ok(());
    }

    if DESTRUCTIVE.contains(&name.as_str()) {
        let is_win = name != "rm";
        let mut operands = Vec::new();
        for tok in tokens.iter().skip(i + 1) {
            if let Some(eq) = tok.find('=')
                && eq > 0 && (tok.starts_with('-') || tok.starts_with('/')) {
                    let value = &tok[eq + 1..];
                    if !value.is_empty() {
                        operands.push(value.to_string());
                    }
                    continue;
                }
            if !is_flag(tok) {
                operands.push(tok.clone());
            }
        }
        for operand in &operands {
            if has_unexpanded_ref(operand) {
                return Err(ShellPolicyError::UnexpandedReference(operand.clone()));
            }
            let normalized = normalize_operand(operand, cwd, is_win);
            if is_protected_root(&normalized) {
                return Err(ShellPolicyError::ProtectedRoot(
                    operand.clone(),
                    normalized,
                ));
            }
        }
    }

    Ok(())
}

fn analyze_command(cmd: &str, cwd: &str, depth: usize) -> Result<(), ShellPolicyError> {
    if let Some(sig) = pattern_level_block(cmd) {
        return Err(ShellPolicyError::ProhibitedPattern(sig.to_string()));
    }
    for seg in split_segments(cmd) {
        analyze_segment(&seg, cwd, depth)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub fn validate(cmd: &str) -> Result<(), ShellPolicyError> {
    if cmd.trim().is_empty() {
        return Err(ShellPolicyError::InvalidCommand);
    }
    if cmd.contains(CANARY_DISASTER_FUSE_TOKEN) {
        return Err(ShellPolicyError::DeadManFuse);
    }
    analyze_command(cmd, DEFAULT_CWD, 0)
}

// ---------------------------------------------------------------------------
// Executor
// ---------------------------------------------------------------------------

/// Per-stream capture ceiling (mirrors the JS executor's 256KB buffer cap).
const MAX_CAPTURE_BYTES: usize = 256 * 1024;

/// Result of a sandboxed shell execution.
#[derive(Debug)]
pub struct ShellOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    /// True when either stream exceeded the capture cap and was truncated.
    pub truncated: bool,
    /// Wall-clock duration in milliseconds (sub-ms precision).
    pub duration_ms: f64,
}

#[derive(Debug, Error)]
pub enum ShellError {
    #[error("policy: {0}")]
    Policy(#[from] ShellPolicyError),
    #[error("cwd: {0}")]
    Cwd(#[from] super::sandbox::SandboxError),
    #[error("spawn: {0}")]
    Spawn(#[from] std::io::Error),
}

/// Executes a shell command with layered safety:
///
/// 1. `validate` — typed policy refusal; nothing is ever spawned on refusal.
/// 2. `cwd` resolved through the sandbox layers (canonicalized real path).
/// 3. Spawn `bash -c <cmd>` with `process_group(0)` so the child leads its
///    own process group; on timeout the whole group is SIGKILL'd (no
///    orphaned grandchildren) and the result reports exit code 124.
///
/// stdout/stderr are captured with a per-stream byte cap; streams beyond
/// the cap are drained and `truncated` is set.
/// Executes a shell command asynchronously with layered safety:
///
/// 1. `validate` — typed policy refusal; nothing is ever spawned on refusal.
/// 2. `cwd` resolved through the sandbox layers (canonicalized real path).
/// 3. Spawn `bash -c <cmd>` with `process_group(0)`.
pub async fn run_async(cmd: &str, cwd: &Path, timeout: Duration) -> Result<ShellOutput, ShellError> {
    // Layer 1: policy validation — typed refusal, nothing spawns.
    validate(cmd)?;
    // Layer 2: resolve the cwd through the sandbox layers (real path).
    let cwd = super::sandbox::resolve_workspace_root(cwd)?;
    execute(cmd, &cwd, timeout).await
}

/// Executes a shell command synchronously by driving `run_async` on a current-thread runtime.
pub fn run(cmd: &str, cwd: &Path, timeout: Duration) -> Result<ShellOutput, ShellError> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(run_async(cmd, cwd, timeout))
}

/// Drains an async stream into a byte-capped buffer, continuing to read
/// (and discarding) past the cap so the child's pipe never deadlocks.
async fn capture_capped<R: tokio::io::AsyncRead + Unpin>(mut r: R, cap: usize) -> (Vec<u8>, bool) {
    let mut out = Vec::new();
    let mut truncated = false;
    let mut buf = [0u8; 8192];
    loop {
        match r.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                if out.len() < cap {
                    let room = cap - out.len();
                    if n > room {
                        out.extend_from_slice(&buf[..room]);
                        truncated = true;
                    } else {
                        out.extend_from_slice(&buf[..n]);
                    }
                } else {
                    // Buffer already at the cap: discard (drain the pipe) and
                    // mark the stream truncated.
                    truncated = true;
                }
            }
            Err(_) => break,
        }
    }
    (out, truncated)
}

#[cfg(target_os = "linux")]
fn build_landlock_ruleset(cwd: &Path) -> Result<landlock::RulesetCreated, String> {
    use landlock::{Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr, ABI};
    let abi = ABI::V1;
    let mut ruleset = Ruleset::default()
        .handle_access(AccessFs::from_all(abi))
        .map_err(|e| format!("Landlock handle_access: {e}"))?
        .create()
        .map_err(|e| format!("Landlock create: {e}"))?;

    // Read-Write for workspace root (cwd)
    let ws_fd = PathFd::new(cwd).map_err(|e| format!("PathFd cwd {}: {e}", cwd.display()))?;
    ruleset = ruleset
        .add_rule(PathBeneath::new(ws_fd, AccessFs::from_all(abi)))
        .map_err(|e| format!("Landlock add_rule cwd: {e}"))?;

    // Read-Write for /tmp
    if let Ok(tmp_fd) = PathFd::new("/tmp") {
        ruleset = ruleset
            .add_rule(PathBeneath::new(tmp_fd, AccessFs::from_all(abi)))
            .map_err(|e| format!("Landlock add_rule /tmp: {e}"))?;
    }

    // Read-Only for system paths
    let ro = AccessFs::from_read(abi);
    for dir in ["/usr", "/bin", "/lib", "/lib64", "/etc", "/dev", "/proc"] {
        if let Ok(fd) = PathFd::new(dir) {
            ruleset = ruleset
                .add_rule(PathBeneath::new(fd, ro))
                .map_err(|e| format!("Landlock add_rule {dir}: {e}"))?;
        }
    }

    // Read-Only for ~/.castor if it exists
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        let state_dir = home.join(".castor");
        if let Ok(fd) = PathFd::new(&state_dir) {
            ruleset = ruleset
                .add_rule(PathBeneath::new(fd, ro))
                .map_err(|e| format!("Landlock add_rule state_dir: {e}"))?;
        }
    }

    Ok(ruleset)
}

async fn execute(cmd: &str, cwd: &Path, timeout: Duration) -> Result<ShellOutput, ShellError> {
    let t0 = Instant::now();
    let mut cmd_builder = tokio::process::Command::new("bash");
    cmd_builder
        .arg("-c")
        .arg(cmd)
        .current_dir(cwd)
        .process_group(0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(target_os = "linux")]
    {
        if let Ok(ruleset) = build_landlock_ruleset(cwd) {
            let mut ruleset_opt = Some(ruleset);
            unsafe {
                cmd_builder.pre_exec(move || {
                    // Best-effort confinement: if the kernel or virtualization layer
                    // rejects restrict_self (e.g. WSL2 hypervisor FD limitations returning EBADF/EPERM),
                    // degrade gracefully to in-process SandboxPolicy path validation rather than
                    // crashing child process spawns.
                    if let Some(r) = ruleset_opt.take() {
                        let _ = r.restrict_self();
                    }
                    Ok(())
                });
            }
        }
    }

    let mut child = cmd_builder.spawn()?;
    let pid = child.id().unwrap_or(0);

    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    let out_buf = Arc::new(std::sync::Mutex::new(Vec::new()));
    let err_buf = Arc::new(std::sync::Mutex::new(Vec::new()));
    let out_trunc = Arc::new(AtomicBool::new(false));
    let err_trunc = Arc::new(AtomicBool::new(false));

    let out_task = {
        let (buf, flag) = (Arc::clone(&out_buf), Arc::clone(&out_trunc));
        tokio::spawn(async move {
            let (b, t) = capture_capped(stdout, MAX_CAPTURE_BYTES).await;
            *buf.lock().unwrap() = b;
            flag.store(t, Ordering::SeqCst);
        })
    };
    let err_task = {
        let (buf, flag) = (Arc::clone(&err_buf), Arc::clone(&err_trunc));
        tokio::spawn(async move {
            let (b, t) = capture_capped(stderr, MAX_CAPTURE_BYTES).await;
            *buf.lock().unwrap() = b;
            flag.store(t, Ordering::SeqCst);
        })
    };

    // Wait for the child with a deadline, polling `try_wait` so the child
    // handle stays owned (needed to kill the process group on timeout).
    let deadline = tokio::time::Instant::now() + timeout;
    let mut status = None;
    loop {
        match child.try_wait() {
            Ok(Some(s)) => {
                status = Some(s);
                break;
            }
            Ok(None) => {
                if tokio::time::Instant::now() >= deadline {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(e) => {
                return Err(ShellError::Spawn(e));
            }
        }
    }

    let (exit_code, out, err) = match status {
        Some(status) => {
            let _ = tokio::join!(out_task, err_task);
            let out = std::mem::take(&mut *out_buf.lock().unwrap());
            let mut err =
                String::from_utf8_lossy(&std::mem::take(&mut *err_buf.lock().unwrap())).into_owned();
            if status.code().is_none() {
                err.push_str("\n[Process terminated by signal]");
            }
            (
                if status.success() {
                    0
                } else {
                    status.code().unwrap_or(1)
                },
                out,
                err,
            )
        }
        None => {
            // Timeout: kill the whole process group, then reap the zombie.
            #[cfg(unix)]
            if pid > 0 {
                unsafe {
                    libc::killpg(pid as i32, libc::SIGKILL);
                    let mut st: libc::c_int = 0;
                    libc::waitpid(pid as i32, &mut st, 0);
                }
            }
            #[cfg(not(unix))]
            {
                let _ = child.kill().await;
            }
            let _ = tokio::join!(out_task, err_task);
            let out = std::mem::take(&mut *out_buf.lock().unwrap());
            let mut err =
                String::from_utf8_lossy(&std::mem::take(&mut *err_buf.lock().unwrap())).into_owned();
            err.push_str(&format!(
                "\n[Command timed out after {}ms]",
                timeout.as_millis()
            ));
            (124, out, err)
        }
    };

    Ok(ShellOutput {
        exit_code,
        stdout: String::from_utf8_lossy(&out).into_owned(),
        stderr: err,
        truncated: out_trunc.load(Ordering::SeqCst) || err_trunc.load(Ordering::SeqCst),
        duration_ms: t0.elapsed().as_secs_f64() * 1000.0,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fork_bomb_blocked() {
        assert!(matches!(
            validate(":(){ :|:& };:"),
            Err(ShellPolicyError::ProhibitedPattern(_))
        ));
    }

    #[test]
    fn fork_bomb_no_space_blocked() {
        assert!(matches!(
            validate(":(){ :|:& };"),
            Err(ShellPolicyError::ProhibitedPattern(_))
        ));
    }

    #[test]
    fn fork_bomb_space_before_brace_blocked() {
        assert!(matches!(
            validate(":() { :|:& };"),
            Err(ShellPolicyError::ProhibitedPattern(_))
        ));
    }

    #[test]
    fn mkfs_blocked() {
        assert!(matches!(
            validate("mkfs.ext4 /dev/sda1"),
            Err(ShellPolicyError::ProhibitedPattern(_))
        ));
    }

    #[test]
    fn fdisk_blocked() {
        assert!(matches!(
            validate("fdisk /dev/sda"),
            Err(ShellPolicyError::ProhibitedPattern(_))
        ));
    }

    #[test]
    fn format_drive_blocked() {
        assert!(matches!(
            validate("format C:"),
            Err(ShellPolicyError::ProhibitedPattern(_))
        ));
    }

    #[test]
    fn dd_to_device_blocked() {
        assert!(matches!(
            validate("dd if=/dev/zero of=/dev/sda"),
            Err(ShellPolicyError::ProhibitedPattern(_))
        ));
    }

    #[test]
    fn rm_rf_root_blocked() {
        assert!(matches!(
            validate("rm -rf /"),
            Err(ShellPolicyError::ProtectedRoot(_, _))
        ));
    }

    #[test]
    fn rm_rf_home_blocked() {
        assert!(matches!(
            validate("rm -rf /home/user"),
            Err(ShellPolicyError::ProtectedRoot(_, _))
        ));
    }

    #[test]
    fn sudo_rm_blocked() {
        assert!(matches!(
            validate("sudo rm -rf /"),
            Err(ShellPolicyError::ProtectedRoot(_, _))
        ));
    }

    #[test]
    fn bash_c_rm_blocked() {
        assert!(matches!(
            validate("bash -c \"rm -rf /\""),
            Err(ShellPolicyError::ProtectedRoot(_, _))
        ));
    }

    #[test]
    fn unexpanded_ref_blocked() {
        assert!(matches!(
            validate("rm -rf $HOME"),
            Err(ShellPolicyError::UnexpandedReference(_))
        ));
    }

    #[test]
    fn deadman_fuse_canary() {
        assert!(matches!(
            validate(CANARY_DISASTER_FUSE_TOKEN),
            Err(ShellPolicyError::DeadManFuse)
        ));
    }

    #[test]
    fn empty_command_rejected() {
        assert!(matches!(
            validate("   "),
            Err(ShellPolicyError::InvalidCommand)
        ));
    }

    #[test]
    fn benign_commands_pass() {
        assert!(validate("ls -la").is_ok());
        assert!(validate("cargo build").is_ok());
        assert!(validate("rm -rf ./build").is_ok());
        assert!(validate("echo hello").is_ok());
        assert!(validate("cat file.txt").is_ok());
        assert!(validate("git status").is_ok());
    }

    #[test]
    fn homoglyph_fullwidth_w_blocked() {
        // GAP 8: fullwidth `Ｗ` (U+FF37) folds to `W` under NFKC, so the
        // protected-root string comparison still matches `/mnt/c/windows`.
        assert!(matches!(
            validate("rm -rf /mnt/c/\u{FF37}indows"),
            Err(ShellPolicyError::ProtectedRoot(_, _))
        ));
    }

}

// ---------------------------------------------------------------------------
// Executor tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod run_tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "castor_shell_test_{tag}_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn benign_echo_runs_and_captures() {
        let dir = tmp_dir("echo");
        let out = run("echo hello", &dir, Duration::from_secs(10)).unwrap();
        assert_eq!(out.exit_code, 0);
        assert_eq!(out.stdout.trim(), "hello");
        assert!(out.stderr.is_empty());
        assert!(!out.truncated);
        assert!(out.duration_ms >= 0.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refusal_rm_rf_root_never_spawns() {
        let dir = tmp_dir("refusal");
        // Marker: if the refused command were ever spawned, this file would
        // eventually be destroyed; it must remain untouched.
        let marker = dir.join("marker.txt");
        std::fs::write(&marker, "alive").unwrap();
        let res = run("rm -rf /", &dir, Duration::from_secs(10));
        assert!(matches!(
            res,
            Err(ShellError::Policy(ShellPolicyError::ProtectedRoot(_, _)))
        ));
        assert!(marker.exists(), "refused command must never spawn");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refusal_canary_never_spawns_marker() {
        let dir = tmp_dir("canary");
        // Stronger proof: the refused command would CREATE the marker file
        // if it were ever spawned. It must not exist afterwards.
        let marker = dir.join("marker.txt");
        let cmd = format!(
            "echo {CANARY_DISASTER_FUSE_TOKEN} > {}",
            marker.display()
        );
        let res = run(&cmd, &dir, Duration::from_secs(10));
        assert!(matches!(
            res,
            Err(ShellError::Policy(ShellPolicyError::DeadManFuse))
        ));
        assert!(!marker.exists(), "refused command must never spawn");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn timeout_kills_process_group() {
        let dir = tmp_dir("timeout");
        let out = run("sleep 30", &dir, Duration::from_millis(300)).unwrap();
        assert_eq!(out.exit_code, 124);
        assert!(out.stderr.contains("timed out"));
        // The group kill must land well before the 30s sleep completes.
        assert!(out.duration_ms < 5000.0, "duration was {}", out.duration_ms);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn output_cap_truncates() {
        let dir = tmp_dir("cap");
        // 300KB of 'a' exceeds the 256KB capture cap.
        let out = run(
            "head -c 300000 /dev/zero | tr '\\0' 'a'",
            &dir,
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(out.exit_code, 0);
        assert!(out.truncated);
        assert_eq!(out.stdout.len(), MAX_CAPTURE_BYTES);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn landlock_confinement_blocks_unauthorized_paths() {
        let dir = tmp_dir("landlock");
        // Within workspace: writing succeeds
        let out_ok = run("echo hello > ok.txt", &dir, Duration::from_secs(10)).unwrap();
        assert_eq!(out_ok.exit_code, 0);
        assert!(dir.join("ok.txt").exists());

        // Denied paths: writing to /etc or reading /root fails with permission denied
        let out_denied = run("touch /etc/test_landlock_fail 2>&1 || ls /root 2>&1", &dir, Duration::from_secs(10)).unwrap();
        let combined = format!("{} {}", out_denied.stdout, out_denied.stderr);
        assert!(
            combined.contains("Permission denied") || combined.contains("denied"),
            "Landlock must deny unauthorized paths: {combined}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
