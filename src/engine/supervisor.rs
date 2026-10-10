//! Detached llama.cpp process owner with authenticated loopback control.

use std::collections::VecDeque;
#[cfg(unix)]
use std::io::Read;
use std::io::{self, Write};
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, Command};

use super::lifecycle::{EngineLifecycle, LifecycleError, llama_readiness};
use crate::config::Config;
use crate::state::StateDir;

const OWNER_FILE: &str = "llama-owner.json";
const EXIT_FILE: &str = "llama-exit.json";
const CONTROL_TIMEOUT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(100);

#[derive(Debug, Serialize, Deserialize)]
struct Owner {
    pid: u32,
    identity: String,
    endpoint: String,
    model: String,
    control_port: u16,
    token: String,
}

#[derive(Serialize, Deserialize)]
struct Control {
    token: String,
    action: String,
}

fn read_owner(state: &StateDir) -> Result<Option<Owner>, LifecycleError> {
    match std::fs::read(state.root().join(OWNER_FILE)) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| LifecycleError::Stop(format!("invalid llama.cpp ownership record: {e}"))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

fn exit_detail(state: &StateDir) -> Result<Option<String>, LifecycleError> {
    match std::fs::read_to_string(state.root().join(EXIT_FILE)) {
        Ok(raw) => serde_json::from_str(&raw)
            .map(Some)
            .map_err(|e| LifecycleError::Stop(format!("invalid supervisor exit record: {e}"))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn atomic_json(path: &Path, value: &impl Serialize) -> io::Result<()> {
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    let result = (|| {
        serde_json::to_writer(&mut file, value)?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        remove_if_present(&temporary)?;
    }
    result
}

/// OS birth identity prevents a reused PID from becoming a process owner.
fn process_identity(pid: u32) -> io::Result<String> {
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
        let fields = stat
            .rsplit_once(')')
            .ok_or_else(|| io::Error::other("invalid process stat"))?
            .1;
        let start = fields
            .split_whitespace()
            .nth(19)
            .ok_or_else(|| io::Error::other("missing process birth time"))?;
        let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
        Ok(format!("{}:{start}", boot.trim()))
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
        use windows_sys::Win32::System::Threading::{
            GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return Err(io::Error::last_os_error());
            }
            let mut creation: FILETIME = std::mem::zeroed();
            let mut exit: FILETIME = std::mem::zeroed();
            let mut kernel: FILETIME = std::mem::zeroed();
            let mut user: FILETIME = std::mem::zeroed();
            let result = GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user);
            let error = if result == 0 {
                Some(io::Error::last_os_error())
            } else {
                None
            };
            CloseHandle(handle);
            if let Some(error) = error {
                return Err(error);
            }
            Ok(format!(
                "{}:{}",
                creation.dwHighDateTime, creation.dwLowDateTime
            ))
        }
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = pid;
        Err(io::Error::other(
            "managed llama.cpp requires Windows or Linux",
        ))
    }
}

fn token() -> io::Result<String> {
    let mut bytes = [0u8; 32];
    #[cfg(unix)]
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    #[cfg(windows)]
    {
        #[link(name = "bcrypt")]
        unsafe extern "system" {
            fn BCryptGenRandom(
                handle: *mut std::ffi::c_void,
                buffer: *mut u8,
                size: u32,
                flags: u32,
            ) -> i32;
        }
        let status = unsafe {
            BCryptGenRandom(
                std::ptr::null_mut(),
                bytes.as_mut_ptr(),
                bytes.len() as u32,
                2,
            )
        };
        if status < 0 {
            return Err(io::Error::other(format!(
                "BCryptGenRandom failed: {status}"
            )));
        }
    }
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn verify(owner: &Owner, config: &Config) -> Result<(), LifecycleError> {
    if super::backend::api_url(config, "")
        .map_err(LifecycleError::Readiness)?
        .trim_end_matches('/')
        != owner.endpoint
        || config.model.as_deref() != Some(owner.model.as_str())
    {
        return Err(LifecycleError::Stop(
            "llama.cpp ownership endpoint/model does not match configuration".into(),
        ));
    }
    let identity = process_identity(owner.pid).map_err(|e| {
        LifecycleError::Stop(format!("stale llama.cpp supervisor {}: {e}", owner.pid))
    })?;
    if identity != owner.identity {
        return Err(LifecycleError::Stop(
            "stale llama.cpp ownership: supervisor PID birth identity changed".into(),
        ));
    }
    Ok(())
}

async fn control(owner: &Owner, action: &str) -> Result<(), LifecycleError> {
    let request = Control {
        token: owner.token.clone(),
        action: action.into(),
    };
    let result = tokio::time::timeout(CONTROL_TIMEOUT, async {
        let mut socket =
            TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, owner.control_port)).await?;
        let mut body = serde_json::to_vec(&request)?;
        body.push(b'\n');
        socket.write_all(&body).await?;
        let mut reply = String::new();
        let count = BufReader::new(socket).read_line(&mut reply).await?;
        if count == 0 || reply.trim() != "ok" {
            return Err(io::Error::other(format!(
                "supervisor control failed: {}",
                reply.trim()
            )));
        }
        Ok::<(), io::Error>(())
    })
    .await
    .map_err(|e| LifecycleError::Stop(format!("supervisor control timed out: {e}")))?;
    result.map_err(LifecycleError::Io)
}

pub(crate) async fn stop(config: &Config, state: &StateDir) -> Result<(), LifecycleError> {
    let owner = read_owner(state)?.ok_or_else(|| {
        LifecycleError::Stop(
            "no owned llama.cpp server; stop an external server yourself or configure stop_command"
                .into(),
        )
    })?;
    verify(&owner, config)?;
    control(&owner, "stop").await
}

pub(crate) async fn start(
    config: &Config,
    state: &StateDir,
    timeout: Duration,
    client: &reqwest::Client,
) -> Result<(), LifecycleError> {
    let endpoint = super::backend::api_url(config, "")
        .map_err(LifecycleError::Readiness)?
        .trim_end_matches('/')
        .to_string();
    let model = config
        .model
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| LifecycleError::Readiness("missing model in config".into()))?;
    if let Some(owner) = read_owner(state)? {
        // A stale record is diagnostic evidence, not permission to launch or kill.
        verify(&owner, config)?;
        return Err(LifecycleError::Stop("an owned llama.cpp supervisor exists but the engine is unhealthy; stop it before restarting".into()));
    }
    state.ensure()?;
    remove_if_present(&state.root().join(EXIT_FILE))?;
    let nonce = token()?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .arg("__engine_supervisor")
        .arg("--state-dir")
        .arg(state.root())
        .arg("--endpoint")
        .arg(&endpoint)
        .arg("--model")
        .arg(model)
        .arg("--launch-command")
        .arg(
            config
                .launch_command
                .as_deref()
                .ok_or(LifecycleError::NoLaunchCommand)?,
        )
        .env("CASTOR_SUPERVISOR_TOKEN", &nonce)
        .arg("--boot-timeout-ms")
        .arg(
            timeout
                .saturating_add(CONTROL_TIMEOUT)
                .as_millis()
                .to_string(),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    let mut supervisor = super::detached::spawn(&mut command)?;
    let deadline = tokio::time::Instant::now() + timeout;
    let mut detail = "waiting for llama.cpp supervisor".to_string();
    loop {
        if let Some(owner) = read_owner(state)? {
            if owner.token != nonce {
                return Err(LifecycleError::Stop(
                    "supervisor ownership changed during startup".into(),
                ));
            }
            match llama_readiness(config, client).await {
                Ok(()) => {
                    verify(&owner, config)?;
                    return control(&owner, "ready").await;
                }
                Err(error) => detail = error,
            }
        }
        if let Some(status) = supervisor.try_wait()? {
            if let Some(exit) = exit_detail(state)? {
                detail.push_str(&format!("\n{exit}"));
            }
            return Err(LifecycleError::Readiness(format!(
                "llama.cpp supervisor exited {status}: {detail}"
            )));
        }
        if tokio::time::Instant::now() >= deadline {
            if let Some(owner) = read_owner(state)? {
                verify(&owner, config)?;
                control(&owner, "stop").await?;
            }
            // The supervisor also enforces its own startup deadline.
            tokio::time::timeout(CONTROL_TIMEOUT, supervisor.wait())
                .await
                .map_err(|e| LifecycleError::Stop(format!("startup cleanup timed out: {e}")))??;
            if let Some(exit) = exit_detail(state)? {
                detail.push_str(&format!("\n{exit}"));
            }
            return Err(LifecycleError::BootTimeout {
                secs: timeout.as_secs(),
                detail,
            });
        }
        tokio::time::sleep(POLL).await;
    }
}

async fn terminate(child: &mut Child) -> io::Result<()> {
    if child.try_wait()?.is_some() {
        return Ok(());
    }
    if let Some(pid) = child.id() {
        #[cfg(unix)]
        {
            let result = unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
            if result != 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    return Err(error);
                }
            }
        }
        #[cfg(windows)]
        {
            let result = Command::new("taskkill")
                .args(["/F", "/T", "/PID", &pid.to_string()])
                .creation_flags(0x08000000)
                .output()
                .await?;
            if !result.status.success() && child.try_wait()?.is_none() {
                return Err(io::Error::other(format!(
                    "taskkill exited {}: {}",
                    result.status,
                    String::from_utf8_lossy(&result.stderr)
                )));
            }
        }
    }
    child.wait().await?;
    Ok(())
}

/// Internal CLI entrypoint; the supervisor retains the child across caller exit.
pub(crate) async fn run(
    state: StateDir,
    endpoint: String,
    model: String,
    launch: String,
    nonce: String,
    timeout_ms: u64,
) -> Result<(), LifecycleError> {
    state.ensure()?;
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
    let owner = Owner {
        pid: std::process::id(),
        identity: process_identity(std::process::id())?,
        endpoint,
        model,
        control_port: listener.local_addr()?.port(),
        token: nonce,
    };
    let mut command = EngineLifecycle::shell_command(&launch);
    command.env_remove("CASTOR_SUPERVISOR_TOKEN");
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    command.process_group(0);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            atomic_json(
                &state.root().join(EXIT_FILE),
                &format!("launch failed: {e}"),
            )?;
            return Err(e.into());
        }
    };
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("missing engine stderr pipe"))?;
    let tail = Arc::new(Mutex::new(VecDeque::new()));
    let captured = tail.clone();
    let reader = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Some(line) = lines.next_line().await? {
            let mut tail = captured.lock().unwrap();
            tail.push_back(line);
            if tail.len() > 50 {
                tail.pop_front();
            }
        }
        Ok::<(), io::Error>(())
    });
    if let Err(e) = atomic_json(&state.root().join(OWNER_FILE), &owner) {
        terminate(&mut child).await?;
        return Err(e.into());
    }
    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
    let mut ready = false;
    let mut stop_reply = None;
    let result: io::Result<String> = async {
        loop {
            tokio::select! {
                status = child.wait() => return Ok(format!("engine exited {}", status?)),
                accepted = listener.accept() => {
                    let (socket, _) = accepted?;
                    let mut socket = BufReader::new(socket);
                    let mut bytes = Vec::new();
                    use tokio::io::AsyncReadExt;
                    let read = tokio::time::timeout(Duration::from_secs(1), (&mut socket).take(1024).read_until(b'\n', &mut bytes)).await;
                    let request = match read {
                        Ok(Ok(_)) => serde_json::from_slice::<Control>(&bytes).ok(),
                        Ok(Err(e)) => {
                            tracing::warn!(error = %e, "supervisor control connection failed");
                            None
                        }
                        Err(_) => None,
                    };
                    if let Some(request) = request && request.token == owner.token {
                        match request.action.as_str() {
                            "ready" => { ready = true; socket.get_mut().write_all(b"ok\n").await?; }
                            "stop" => {
                                terminate(&mut child).await?;
                                stop_reply = Some(socket.into_inner());
                                return Ok("engine stopped".into());
                            }
                            _ => { socket.get_mut().write_all(b"unknown action\n").await?; }
                        }
                    }
                }
                _ = tokio::time::sleep_until(deadline), if !ready => return Ok("engine startup deadline expired".into()),
            }
        }
    }.await;
    terminate(&mut child).await?;
    reader
        .await
        .map_err(|e| io::Error::other(format!("stderr reader failed: {e}")))??;
    let stderr = tail
        .lock()
        .unwrap()
        .iter()
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    let detail = match &result {
        Ok(message) => message.clone(),
        Err(e) => e.to_string(),
    };
    atomic_json(
        &state.root().join(EXIT_FILE),
        &format!("{detail}\n{stderr}"),
    )?;
    remove_if_present(&state.root().join(OWNER_FILE))?;
    if let Some(mut socket) = stop_reply {
        socket.write_all(b"ok\n").await?;
    }
    result.map(|_| ()).map_err(LifecycleError::Io)
}
