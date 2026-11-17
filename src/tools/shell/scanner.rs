//! Lexical command scanning and security validation rules.
//!
//! Classifies command strings against prohibited destructive patterns,
//! protected roots, unexpanded shell references, and synthetic canaries.

use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ShellPolicyError {
    #[error("InvalidCommandError: Shell command must be a non-empty string")]
    InvalidCommand,
    #[error(
        "CommandSecurityError: Execution blocked. Command matches prohibited destructive pattern: {0}. \
         The command was not run. This is a binding security decision, not a transient failure. \
         Do NOT bypass it with privilege escalation, backgrounding, or shell wrappers \
         (e.g. `sudo`, `nohup`, `env`, `sh -c`). If the task genuinely cannot proceed without \
         this operation, stop and report the constraint in your final report to the Lead \
         Architect / user."
    )]
    ProhibitedPattern(String),
    #[error(
        "CommandSecurityError: Execution blocked. Unexpanded shell reference in target '{0}'. \
         The command was not run. Expand the reference to a concrete, in-workspace path and retry \
         — do not substitute an absolute out-of-workspace path or another tool to reach the same \
         target; if the target is outside the workspace, stop and report the constraint to the \
         Lead Architect / user."
    )]
    UnexpandedReference(String),
    #[error(
        "CommandSecurityError: Execution blocked. Target '{0}' resolves to protected root: {1}. \
         The command was not run. These system directories are protected from destructive \
         operations; this is a binding security decision, not a transient failure. \
         Do NOT bypass it (no privilege escalation, backgrounding, wrappers, or an alternative \
         command targeting the same protected path). If the task cannot proceed without touching \
         that path, stop and report the constraint in your final report to the Lead \
         Architect / user."
    )]
    ProtectedRoot(String, String),
    #[error(
        "FatalDeadManFuseError: Execution halted by dead-man fuse (synthetic canary tripped). The command was not run. Do not retry this command — report the trigger to the Lead Architect / user."
    )]
    DeadManFuse,
}

pub const CANARY_DISASTER_FUSE_TOKEN: &str = "__CANARY_TRIGGER_DISASTER_FUSE__";

/// Fallback working directory and user home used by the single-argument
/// [`validate`] (and any caller that does not supply a live context). The
/// live executor passes the actual workspace root and user home through
/// [`validate_with_context`] instead of these constants.
pub(crate) const DEFAULT_CWD: &str = "/workspace";
pub(crate) const DEFAULT_HOME: &str = "/home/user";
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
    i == 0 || is_delimiter_boundary(chars[i - 1]) || chars[i - 1] == '$'
}

/// Whether a *byte* is one of the single-byte shell delimiter / metacharacters
/// (all delimiters are ASCII, so this is well-defined on a raw byte).
///
/// Used by the zero-allocation group / case / substitution scanners, which walk
/// `cmd.char_indices()` (yielding *byte* positions) and inspect neighbor bytes
/// directly instead of a collected `Vec<char>`. A neighbor byte is a delimiter
/// character iff it is one of these ASCII bytes: a byte ≥ 0x80 is a UTF-8
/// continuation byte and can never be a delimiter, so the byte test agrees with
/// the char-level [`is_delimiter_boundary`] on exactly the positions those
/// scanners visit.
fn is_delimiter_byte(b: u8) -> bool {
    matches!(
        b,
        b' ' | b'\t' | b';' | b'&' | b'|' | b'(' | b')' | b'{' | b'}' | b'<' | b'>' | b'\n' | b'\r'
    )
}

/// Whether the `{`/`}` whose first byte is at `i` (a char boundary in `s`) is
/// a **standalone word** — a word of its own — in the zero-allocation scanners
/// that walk `s.char_indices()`. Mirrors the char-level
/// [`is_standalone_brace`]: the byte just before the char and the byte just
/// after it (`i + len`) must each be a delimiter (or the string edge). A byte
/// ≥ 0x80 is a UTF-8 continuation byte and can never be a delimiter, so the
/// byte test agrees with the char test at exactly the positions those scanners
/// visit.
fn is_standalone_brace_byte(s: &str, i: usize, len: usize) -> bool {
    let word_start = i == 0 || is_delimiter_byte(s.as_bytes()[i - 1]);
    let word_end = i + len >= s.len() || is_delimiter_byte(s.as_bytes()[i + len]);
    word_start && word_end
}

/// Whether the `(` whose first byte is at `i` (a char boundary in `s`) is a
/// structural opener in the zero-allocation scanners: it starts a word,
/// follows a delimiter, or follows `$` (as in `$(...)`). Mirrors the
/// char-level [`is_structural_open`] on bytes.
fn is_structural_open_byte(s: &str, i: usize) -> bool {
    i == 0 || is_delimiter_byte(s.as_bytes()[i - 1]) || s.as_bytes()[i - 1] == b'$'
}

/// Splits a command into segments on the top-level shell separators
/// `&&`, `&`, `||`, `|`, `;`, newlines, and `)` terminators (group ends,
/// `$(...)` ends, and unmatched `case` pattern closers).
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
        let closes_outer = ch == ')' && paren_depth > 0;
        match ch {
            '(' if is_structural_open(&chars, i) => paren_depth += 1,
            ')' if closes_outer => paren_depth -= 1,
            '{' if is_standalone_brace(&chars, i) => brace_depth += 1,
            '}' if is_standalone_brace(&chars, i) => brace_depth = brace_depth.saturating_sub(1),
            _ => {}
        }
        let terminator =
            ch == ')' && (closes_outer || (i == 0 || is_delimiter_boundary(chars[i - 1])));
        let at_top_level = paren_depth == 0 && brace_depth == 0;
        if at_top_level && (matches!(ch, '&' | '|' | ';' | '\n' | '\r') || terminator) {
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
    let chars: Vec<char> = name.chars().collect();
    let (kind, tail) = match &chars[..] {
        c if c.len() >= 3 && c[0] == 's' && c[1] == 'd' => ("sd", &c[2..]),
        c if c.len() >= 3 && c[0] == 'h' && c[1] == 'd' => ("hd", &c[2..]),
        c if c.len() >= 3 && c[0] == 'v' && c[1] == 'd' => ("vd", &c[2..]),
        c if c.len() >= 5 && c[0] == 'n' && c[1] == 'v' && c[2] == 'm' && c[3] == 'e' => {
            ("nvme", &c[4..])
        }
        _ => return false,
    };
    match kind {
        "sd" | "hd" | "vd" => !tail.is_empty() && tail.iter().all(|c| c.is_ascii_lowercase()),
        "nvme" => {
            let d1: Vec<char> = tail
                .iter()
                .take_while(|c| c.is_ascii_digit())
                .copied()
                .collect();
            let rem = &tail[d1.len()..];
            let ok_n = rem.first().copied() == Some('n');
            let d2: Vec<char> = rem
                .iter()
                .skip(1)
                .take_while(|c| c.is_ascii_digit())
                .copied()
                .collect();
            ok_n && !d1.is_empty() && !d2.is_empty() && rem.len() == 1 + d2.len()
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

fn analyze_group(cmd: &str, cwd: &str, home: &str, depth: usize) -> Result<(), ShellPolicyError> {
    if depth >= MAX_GROUP_DEPTH {
        return Ok(());
    }

    let mut quote: Option<char> = None;
    let mut start: Option<(usize, char)> = None;
    let mut close: Option<usize> = None;
    let mut paren: usize = 0;
    let mut brace: usize = 0;
    for (i, ch) in cmd.char_indices() {
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
            '{' if is_standalone_brace_byte(cmd, i, 1) => {
                if start.is_none() && paren == 0 && brace == 0 {
                    start = Some((i, '{'));
                }
                brace += 1;
            }
            '}' if is_standalone_brace_byte(cmd, i, 1) => brace = brace.saturating_sub(1),
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
        return Ok(());
    };

    let prefix = &cmd[..s];
    let body = &cmd[s + 1..c];
    let suffix = &cmd[c + 1..];

    if !prefix.trim().is_empty() {
        analyze_command(prefix, cwd, home, depth + 1)?;
    }
    let inner = body.trim().trim_matches(';').trim();
    if !inner.is_empty() {
        analyze_command(inner, cwd, home, depth + 1)?;
    }
    if !suffix.trim().is_empty() {
        analyze_command(suffix, cwd, home, depth + 1)?;
    }
    Ok(())
}

fn analyze_case(cmd: &str, cwd: &str, home: &str, depth: usize) -> Result<(), ShellPolicyError> {
    if depth >= MAX_GROUP_DEPTH {
        return Ok(());
    }
    if !cmd.trim_start().to_ascii_lowercase().starts_with("case ") {
        return Ok(());
    }

    let lower: String = cmd.to_ascii_lowercase();
    let mut in_idx: Option<usize> = None;
    for (i, ch) in lower.char_indices() {
        if ch == 'i' && lower[i..].starts_with("in") {
            let b0 = lower.floor_char_boundary(i);
            let b1 = lower.floor_char_boundary(i + 2);
            if (i == 0 || is_delimiter_byte(lower.as_bytes()[b0 - 1]))
                && (i + 2 >= lower.len() || is_delimiter_byte(lower.as_bytes()[b1]))
            {
                in_idx = Some(b1);
                break;
            }
        }
    }
    let Some(in_idx) = in_idx else {
        return Ok(());
    };

    let tail = &lower[in_idx..];
    let mut body_end = tail.len();
    for (i, ch) in tail.char_indices() {
        if ch == 'e' && tail[i..].starts_with("esac") {
            let b0 = tail.floor_char_boundary(i);
            let b1 = tail.floor_char_boundary(i + 4);
            if (i == 0 || is_delimiter_byte(tail.as_bytes()[b0 - 1]))
                && (i + 4 >= tail.len() || is_delimiter_byte(tail.as_bytes()[b1]))
            {
                body_end = b0;
            }
        }
    }
    let region = &tail[..body_end];

    let mut quote: Option<char> = None;
    let mut paren: usize = 0;
    let mut brace: usize = 0;
    let mut body_start: Option<usize> = None;
    let mut i = 0;
    while i < region.len() {
        let ch = region[i..].chars().next().unwrap();
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            }
            i += ch.len_utf8();
            continue;
        }
        if ch == '"' || ch == '\'' {
            quote = Some(ch);
            i += 1;
            continue;
        }
        match ch {
            '(' if is_structural_open_byte(region, i) => paren += 1,
            ')' if paren > 0 => paren -= 1,
            '{' if is_standalone_brace_byte(region, i, 1) => brace += 1,
            '}' if is_standalone_brace_byte(region, i, 1) => brace = brace.saturating_sub(1),
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
            if let Some(s) = body_start.take() {
                flush_body(s, i)?;
            }
            body_start = Some(i + 1);
        } else if at_top && ch == ';' && region[i + 1..].starts_with(';') {
            if let Some(s) = body_start.take() {
                flush_body(s, i)?;
            }
            i += 1;
        }
        i += ch.len_utf8();
    }
    if let Some(s) = body_start {
        let body = &region[s..];
        if !body.trim().is_empty() {
            analyze_command(body, cwd, home, depth + 1)?;
        }
    }
    Ok(())
}

fn analyze_substitutions(
    cmd: &str,
    cwd: &str,
    home: &str,
    depth: usize,
) -> Result<(), ShellPolicyError> {
    if depth >= MAX_GROUP_DEPTH {
        return Ok(());
    }
    let mut quote: Option<char> = None;
    let mut i = 0;
    while i < cmd.len() {
        let ch = cmd[i..].chars().next().unwrap();
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            }
            i += ch.len_utf8();
            continue;
        }
        if ch == '"' || ch == '\'' {
            quote = Some(ch);
            i += 1;
            continue;
        }
        if ch == '$' && cmd[i + 1..].starts_with('(') {
            let mut paren = 1usize;
            let mut j = i + 2;
            while j < cmd.len() && paren > 0 {
                let cj = cmd[j..].chars().next().unwrap();
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
                j += cj.len_utf8();
            }
            if paren == 0 && j <= cmd.len() {
                let body = &cmd[i + 2..j - 1];
                if !body.trim().is_empty() {
                    analyze_command(body, cwd, home, depth + 1)?;
                }
                i = j;
                continue;
            }
            break;
        }
        if ch == '`' {
            let mut j = i + 1;
            while j < cmd.len() {
                let cj = cmd[j..].chars().next().unwrap();
                if cj == '`' {
                    break;
                }
                j += cj.len_utf8();
            }
            if j < cmd.len() {
                let body = &cmd[i + 1..j];
                if !body.trim().is_empty() {
                    analyze_command(body, cwd, home, depth + 1)?;
                }
                i = j + 1;
                continue;
            }
            break;
        }
        i += ch.len_utf8();
    }
    Ok(())
}

fn analyze_segment(cmd: &str, cwd: &str, home: &str, depth: usize) -> Result<(), ShellPolicyError> {
    let tokens = tokenize(cmd);
    if tokens.is_empty() {
        return Ok(());
    }

    analyze_group(cmd, cwd, home, depth)?;
    analyze_case(cmd, cwd, home, depth)?;
    analyze_substitutions(cmd, cwd, home, depth)?;

    let mut i = 0;
    while i < tokens.len() {
        let name = command_name(&tokens[i]);
        if name == "env" {
            i += 1;
            while i < tokens.len() && (is_env_assignment(&tokens[i]) || is_flag(&tokens[i])) {
                i += 1;
            }
            if i >= tokens.len() {
                return Ok(());
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
pub fn resolve_home() -> Option<String> {
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
