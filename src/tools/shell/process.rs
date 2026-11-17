//! Cross-platform command spawning, stream capture, and process tree lifecycle management.

use std::path::Path;
#[cfg(target_os = "linux")]
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use thiserror::Error;
use tokio::io::AsyncReadExt;

use super::scanner::ShellPolicyError;

/// Per-stream capture ceiling (256KB buffer cap).
pub(crate) const MAX_CAPTURE_BYTES: usize = 256 * 1024;

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
    Cwd(#[from] crate::tools::sandbox::SandboxError),
    #[error("spawn: {0}")]
    Spawn(#[from] std::io::Error),
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

pub async fn execute(cmd: &str, cwd: &Path, timeout: Duration) -> Result<ShellOutput, ShellError> {
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
