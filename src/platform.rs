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

/// Normalizes a Windows command for execution from WSL.
///
/// When the process runs under WSL, a leading `cmd` or `taskkill` token is
/// rewritten to its explicit `.exe` form
/// (e.g. `taskkill /F /IM llama-server.exe` -> `taskkill.exe /F /IM llama-server.exe`).
/// Otherwise the command is returned unchanged.
pub fn wsl_normalize(cmd: &str) -> String {
    if !is_wsl() {
        return cmd.to_string();
    }
    let trimmed = cmd.trim();
    let first_end = trimmed
        .find(|c: char| c.is_whitespace())
        .unwrap_or(trimmed.len());
    let first = &trimmed[..first_end];
    let rest = trimmed[first_end..].trim_start();
    match first {
        "cmd" => {
            if rest.is_empty() {
                "cmd.exe".to_string()
            } else {
                format!("cmd.exe {rest}")
            }
        }
        "taskkill" => {
            if rest.is_empty() {
                "taskkill.exe".to_string()
            } else {
                format!("taskkill.exe {rest}")
            }
        }
        _ => cmd.to_string(),
    }
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
        assert!(wsl || !has_marker, "is_wsl() inconsistent with WSLInterop marker");
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
            assert_eq!(
                wsl_normalize("cmd /c echo hi"),
                "cmd.exe /c echo hi"
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
}
