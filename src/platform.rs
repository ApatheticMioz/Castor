//! Platform detection and OS-specific process management.

/// Terminate a process and its whole process group.
pub fn kill_process_tree(pid: u32) {
    if pid == 0 {
        return;
    }
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
        libc::kill(pid as i32, libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .output();
    }
}

/// Translates a path to the host environment's native path format.
///
/// In WSL/Linux:
/// If passed a Windows-style path with a drive letter (e.g. `D:\foo\bar` or `d:/foo/bar`),
/// maps it to `/mnt/<drive>/foo/bar`.
///
/// In Windows:
/// Retains native paths as-is.
pub fn to_host_path(p: impl AsRef<std::path::Path>) -> std::path::PathBuf {
    let path = p.as_ref();
    #[cfg(unix)]
    {
        return std::path::PathBuf::from(to_wsl_path(path));
    }
    #[allow(unreachable_code)]
    path.to_path_buf()
}

/// Translates a Windows path with a drive letter to WSL /mnt format (e.g. `D:\foo\bar` -> `/mnt/d/foo/bar`).
pub fn to_wsl_path(p: impl AsRef<std::path::Path>) -> String {
    let s = p.as_ref().to_string_lossy();
    let trimmed = s.trim();
    let bytes = trimmed.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        let drive = (bytes[0] as char).to_ascii_lowercase();
        let rest = if bytes.len() >= 3 && (bytes[2] == b'\\' || bytes[2] == b'/') {
            &trimmed[3..]
        } else {
            &trimmed[2..]
        };
        let normalized_rest = rest.replace('\\', "/");
        let clean_rest = normalized_rest.trim_start_matches('/');
        if clean_rest.is_empty() {
            format!("/mnt/{drive}")
        } else {
            format!("/mnt/{drive}/{clean_rest}")
        }
    } else {
        trimmed.replace('\\', "/")
    }
}

/// Returns the path to the bash executable.
///
/// On Windows, prefers Git for Windows bash (`C:\Program Files\Git\bin\bash.exe`)
/// over Windows' System32 `bash.exe` stub (which redirects to WSL).
pub fn bash_path() -> std::path::PathBuf {
    #[cfg(windows)]
    {
        for candidate in [
            r"C:\Program Files\Git\bin\bash.exe",
            r"C:\Program Files (x86)\Git\bin\bash.exe",
        ] {
            let p = std::path::Path::new(candidate);
            if p.is_file() {
                return p.to_path_buf();
            }
        }
        if let Ok(output) = std::process::Command::new("where.exe")
            .arg("git.exe")
            .output()
            && output.status.success()
        {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                let git_path = std::path::Path::new(line.trim());
                if let Some(git_dir) = git_path.parent().and_then(|p| p.parent()) {
                    let bash_bin = git_dir.join("bin").join("bash.exe");
                    if bash_bin.is_file() {
                        return bash_bin;
                    }
                }
            }
        }
    }
    std::path::PathBuf::from("bash")
}

/// Detects whether the process is running inside WSL.
///
/// Prefers the WSL interop marker
/// (`/proc/sys/fs/binfmt_misc/WSLInterop`); falls back to a substring check
/// for `microsoft` in `/proc/version`. Both artifacts are absent on native
/// Windows or bare Linux, so this returns `false` there.
pub fn is_wsl() -> bool {
    if std::path::Path::new("/proc/sys/fs/binfmt_misc/WSLInterop").exists() {
        return true;
    }
    std::fs::read_to_string("/proc/version")
        .ok()
        .map(|s| s.contains("microsoft"))
        .unwrap_or(false)
}

/// Strip a leading `wsl`/`wsl.exe` interop wrapper (and its options) from a
/// command, returning the inner command to run directly.
///
/// `wsl` takes options *before* the command it executes: `-d`/`--distribution
/// <name>`, `-u`/`--user <name>`, `-e`/`--exec`, and the `--` separator. This
/// consumes the leading `wsl`/`wsl.exe` token plus any of those options so the
/// inner command can be executed directly:
///
///   `wsl -d Ubuntu -- bash -lc "pkill vllm"` -> `bash -lc "pkill vllm"`
///   `wsl -u root pkill -f vllm`              -> `pkill -f vllm`
///   `wsl pkill -f vllm`                       -> `pkill -f vllm`
///
/// Commands that do not start with `wsl`/`wsl.exe` are returned unchanged.
/// The logic is pure (no WSL detection) so it is testable on every platform.
pub fn strip_wsl_wrapper(cmd: &str) -> String {
    let t = cmd.trim();
    let mut tokens: Vec<&str> = t.split_whitespace().collect();
    let Some(first) = tokens.first().copied() else {
        return cmd.to_string();
    };
    if first != "wsl" && first != "wsl.exe" {
        return cmd.to_string();
    }
    tokens.remove(0);
    while let Some(flag) = tokens.first().copied() {
        match flag {
            // Options that consume the following value.
            "-d" | "--distribution" | "-u" | "--user" => {
                tokens.remove(0);
                if !tokens.is_empty() {
                    tokens.remove(0);
                }
            }
            // Flag-only options and the arg-separator / exec markers.
            "-e" | "--exec" | "--" => {
                tokens.remove(0);
            }
            _ => break,
        }
    }
    tokens.join(" ")
}

/// Normalizes a Windows command for execution from WSL.
///
/// When the process runs under WSL, a leading `cmd` or `taskkill` token is
/// rewritten to its explicit `.exe` form
/// (e.g. `taskkill /F /IM llama-server.exe` -> `taskkill.exe /F /IM llama-server.exe`),
/// and a leading `wsl`/`wsl.exe` interop wrapper is stripped so the inner
/// command runs directly (never `sh -c "wsl ..."`). Otherwise the command is
/// returned unchanged.
pub fn wsl_normalize(cmd: &str) -> String {
    if !is_wsl() {
        return cmd.to_string();
    }
    normalize_launch_command(cmd)
}

/// Normalizes a command string for execution under POSIX `sh -c`.
///
/// Prevents command-not-found failures (exit 127) when commands carry Windows backslashes
/// in path segments (e.g. `scripts\wsl\start.sh` or `D:\models\llama.exe`):
/// - Strips leading `wsl` / `wsl.exe` wrappers.
/// - Rewrites `cmd` / `taskkill` to their `.exe` forms under WSL.
/// - Converts Windows backslashes in path segments to forward slashes.
/// - Translates Windows drive letter paths (e.g. `D:\...`) to `/mnt/d/...` under WSL/Linux.
pub fn normalize_launch_command(cmd: &str) -> String {
    let stripped = strip_wsl_wrapper(cmd);
    let trimmed = stripped.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let first_end = trimmed
        .find(|c: char| c.is_whitespace())
        .unwrap_or(trimmed.len());
    let first = &trimmed[..first_end];
    let rest = trimmed[first_end..].trim_start();
    match first {
        "cmd" | "cmd.exe" => {
            let safe_rest = rest.replace('\\', "/");
            let cmd_bin = if is_wsl() { "cmd.exe" } else { first };
            if safe_rest.is_empty() {
                cmd_bin.to_string()
            } else {
                format!("{cmd_bin} {safe_rest}")
            }
        }
        "taskkill" => {
            let tk_bin = if is_wsl() { "taskkill.exe" } else { first };
            if rest.is_empty() {
                tk_bin.to_string()
            } else {
                format!("{tk_bin} {rest}")
            }
        }
        _ => normalize_command_string(&stripped),
    }
}

/// Helper that converts Windows path separators to forward slashes in command strings
/// while preserving intentional shell escape characters (`\"`, `\'`, `\ `).
pub fn normalize_command_string(cmd: &str) -> String {
    let mut out = String::with_capacity(cmd.len());
    let mut chars = cmd.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.peek() {
                Some(&next)
                    if next == ' '
                        || next == '"'
                        || next == '\''
                        || next == '$'
                        || next == '`' =>
                {
                    out.push('\\');
                    out.push(next);
                    chars.next();
                }
                Some(&'\\') => {
                    chars.next();
                    out.push('/');
                }
                Some(_) => {
                    out.push('/');
                }
                None => {
                    out.push('\\');
                }
            }
        } else {
            out.push(ch);
        }
    }
    to_wsl_command_paths(&out)
}

fn to_wsl_command_paths(cmd: &str) -> String {
    let words: Vec<&str> = cmd.split(' ').collect();
    let mut normalized_words = Vec::new();
    for word in words {
        let b = word.as_bytes();
        if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'/' {
            let drive = (b[0] as char).to_ascii_lowercase();
            let path_part = &word[3..];
            normalized_words.push(format!("/mnt/{drive}/{path_part}"));
        } else {
            normalized_words.push(word.to_string());
        }
    }
    normalized_words.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_to_host_path() {
        #[cfg(unix)]
        {
            assert_eq!(
                to_host_path("D:\\LLM_Ecosystem"),
                std::path::PathBuf::from("/mnt/d/LLM_Ecosystem")
            );
            assert_eq!(
                to_host_path("c:/users/apath"),
                std::path::PathBuf::from("/mnt/c/users/apath")
            );
            assert_eq!(
                to_host_path("/mnt/d/LLM_Ecosystem"),
                std::path::PathBuf::from("/mnt/d/LLM_Ecosystem")
            );
            assert_eq!(
                to_host_path("relative/path"),
                std::path::PathBuf::from("relative/path")
            );
        }
        #[cfg(windows)]
        {
            assert_eq!(
                to_host_path("D:\\LLM_Ecosystem"),
                std::path::PathBuf::from("D:\\LLM_Ecosystem")
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_is_wsl_detection() {
        // On this CI host (WSL2) the marker exists; the assertion is guarded so
        // it remains meaningful only where WSL detection is observable.
        let wsl = is_wsl();
        let has_marker = std::path::Path::new("/proc/sys/fs/binfmt_misc/WSLInterop").exists();
        // Detection is consistent with at least one artifact.
        assert!(
            wsl || !has_marker,
            "is_wsl() inconsistent with WSLInterop marker"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_wsl_normalize() {
        // Under WSL, bare `cmd`/`taskkill` tokens get an explicit `.exe`.
        if is_wsl() {
            assert_eq!(wsl_normalize("taskkill"), "taskkill.exe");
            assert_eq!(wsl_normalize("cmd"), "cmd.exe");
            assert_eq!(
                wsl_normalize("taskkill /F /T /PID 1234"),
                "taskkill.exe /F /T /PID 1234"
            );
            assert_eq!(wsl_normalize("cmd /c echo hi"), "cmd.exe /c echo hi");
            assert_eq!(
                wsl_normalize(r"cmd /c scripts\bonsai\start.bat"),
                "cmd.exe /c scripts/bonsai/start.bat"
            );
            // A `wsl` interop wrapper is stripped so the inner command runs
            // directly rather than as `sh -c "wsl ..."`.
            assert_eq!(
                wsl_normalize(r#"wsl -d Ubuntu -- bash -lc "pkill vllm""#),
                r#"bash -lc "pkill vllm""#
            );
            assert_eq!(wsl_normalize("wsl pkill -f vllm"), "pkill -f vllm");
            assert_eq!(
                wsl_normalize("wsl.exe -u root pkill -f vllm"),
                "pkill -f vllm"
            );
            // Non-matching commands are untouched.
            assert_eq!(wsl_normalize("nvidia-smi"), "nvidia-smi");
            assert_eq!(
                wsl_normalize("taskkillX /F /IM foo"),
                "taskkillX /F /IM foo"
            );
        } else {
            // Off-WSL, normalization is the identity.
            assert_eq!(wsl_normalize("taskkill"), "taskkill");
            assert_eq!(wsl_normalize("cmd"), "cmd");
        }
    }

    /// `strip_wsl_wrapper` is pure (no WSL detection) so it is testable on any
    /// platform.
    #[test]
    fn test_strip_wsl_wrapper() {
        // Distribution + separator: options before the inner command are dropped.
        assert_eq!(
            strip_wsl_wrapper(r#"wsl -d Ubuntu -- bash -lc "pkill vllm""#),
            r#"bash -lc "pkill vllm""#
        );
        // Bare wrapper, no options.
        assert_eq!(strip_wsl_wrapper("wsl pkill -f vllm"), "pkill -f vllm");
        // `.exe` form plus a `--user <arg>` option.
        assert_eq!(
            strip_wsl_wrapper("wsl.exe -u root pkill -f vllm"),
            "pkill -f vllm"
        );
        // Long option forms and `--exec`.
        assert_eq!(
            strip_wsl_wrapper("wsl --distribution Ubuntu --user root -- pkill -f vllm"),
            "pkill -f vllm"
        );
        assert_eq!(
            strip_wsl_wrapper("wsl --exec pkill -f vllm"),
            "pkill -f vllm"
        );
        // A `wsl` that is merely a prefix of a longer token is left alone.
        assert_eq!(
            strip_wsl_wrapper("wslx -d Ubuntu -- bash -lc hi"),
            "wslx -d Ubuntu -- bash -lc hi"
        );
        // A command with no wrapper is returned unchanged.
        assert_eq!(
            strip_wsl_wrapper("bash -lc \"pkill vllm\""),
            "bash -lc \"pkill vllm\""
        );
        assert_eq!(strip_wsl_wrapper("nvidia-smi"), "nvidia-smi");
    }

    #[test]
    fn test_normalize_launch_command_paths() {
        let cmd = r"bash scripts\wsl\start_swarm.sh";
        let norm = normalize_launch_command(cmd);
        assert_eq!(norm, "bash scripts/wsl/start_swarm.sh");

        let cmd2 = r"scripts\wsl\start_swarm.sh";
        let norm2 = normalize_launch_command(cmd2);
        assert_eq!(norm2, "scripts/wsl/start_swarm.sh");

        let cmd3 = r"cmd.exe /c scripts\bonsai\start.bat";
        let norm3 = normalize_launch_command(cmd3);
        assert_eq!(norm3, "cmd.exe /c scripts/bonsai/start.bat");

        let cmd4 = r#"echo "hello \"world\"""#;
        let norm4 = normalize_launch_command(cmd4);
        assert_eq!(norm4, r#"echo "hello \"world\"""#);

        let cmd5 = r"D:\LLM_Ecosystem\scripts\start.sh";
        let norm5 = normalize_launch_command(cmd5);
        assert!(norm5.contains("LLM_Ecosystem/scripts/start.sh"));
        assert!(!norm5.contains('\\'));
    }
}
