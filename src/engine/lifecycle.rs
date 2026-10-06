//! Engine lifecycle: boot, canary, wedge detection, heal, stop.
//!
//! Port of the `mcp-castor` `server_lifecycle.js` semantics: a cross-process
//! boot lock, a canary that probes the configured model id, a wedge counter
//! with a serialized heal, and a stop that prefers the configured
//! `stop_command` over a process-group kill.

use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::config::Config;
use crate::state::{AcquireResult, Locks, StateDir, WedgeCounter};

const BOOT_LOCK: &str = "engine_boot";
const HEAL_LOCK: &str = "heal";
const BOOT_LOCK_TTL_MS: u64 = 480_000;
const HEAL_LOCK_TTL_MS: u64 = 300_000;
const CANARY_TIMEOUT: Duration = Duration::from_secs(5);
const BOOT_POLL: Duration = Duration::from_millis(200);
const WEDGE_THRESHOLD: u64 = 3;
const STDERR_TAIL_LINES: usize = 50;

#[derive(Debug, Error)]
pub enum LifecycleError {
    #[error("engine already running")]
    AlreadyRunning,
    #[error("boot lock held by pid {pid}")]
    BootLockHeld { pid: u32 },
    #[error("heal lock held by pid {pid}")]
    HealLockHeld { pid: u32 },
    #[error("engine did not become healthy within {secs}s: {detail}")]
    BootTimeout { secs: u64, detail: String },
    #[error("no launch_command configured")]
    NoLaunchCommand,
    #[error("stop failed: {0}")]
    Stop(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub struct EngineLifecycle {
    config: Config,
    locks: Locks,
    wedge: WedgeCounter,
    client: reqwest::Client,
    child: Mutex<Option<tokio::process::Child>>,
    boot_timeout: Duration,
}

impl EngineLifecycle {
    pub fn new(config: &Config, state: &StateDir) -> Self {
        let client = reqwest::Client::builder()
            .timeout(CANARY_TIMEOUT)
            .build()
            .expect("reqwest client");
        Self {
            config: config.clone(),
            locks: Locks::from_state(state),
            wedge: WedgeCounter::from_state(state, "engine"),
            client,
            child: Mutex::new(None),
            boot_timeout: Duration::from_secs(config.boot_timeout_secs),
        }
    }

    pub fn with_boot_timeout(mut self, d: Duration) -> Self {
        self.boot_timeout = d;
        self
    }

    /// GET /v1/models; healthy iff 200 and the configured model id is listed.
    pub async fn canary(&self) -> bool {
        let Some(model) = &self.config.model else {
            return false;
        };
        let url = format!("http://127.0.0.1:{}/v1/models", self.config.ports.engine);
        let res = match self.client.get(&url).send().await {
            Ok(r) => r,
            Err(_) => return false,
        };
        if res.status() != 200 {
            return false;
        }
        let v: Value = match res.json().await {
            Ok(v) => v,
            Err(_) => return false,
        };
        let Some(ids) = v.get("data").and_then(|d| d.as_array()) else {
            return false;
        };
        ids.iter()
            .any(|m| m.get("id").and_then(|i| i.as_str()) == Some(model.as_str()))
    }

    /// Ensure the engine is running and healthy.
    ///
    /// Fast path: if the canary already passes, returns `Ok(())` immediately.
    /// Otherwise, attempts to boot the engine (or waits for an in-progress boot
    /// if another process holds the boot lock).
    pub async fn ensure_running(&self) -> Result<(), LifecycleError> {
        if self.canary().await {
            return Ok(());
        }
        if self.config.launch_command.is_none() {
            return Err(LifecycleError::NoLaunchCommand);
        }
        match self.boot().await {
            Ok(()) => Ok(()),
            Err(LifecycleError::AlreadyRunning) => Ok(()),
            Err(LifecycleError::BootLockHeld { .. }) => self.wait_for_healthy().await,
            Err(e) => Err(e),
        }
    }

    async fn wait_for_healthy(&self) -> Result<(), LifecycleError> {
        let deadline = tokio::time::Instant::now() + self.boot_timeout;
        while tokio::time::Instant::now() < deadline {
            if self.canary().await {
                return Ok(());
            }
            tokio::time::sleep(BOOT_POLL).await;
        }
        Err(LifecycleError::BootTimeout {
            secs: self.boot_timeout.as_secs(),
            detail: "timed out waiting for concurrent engine boot".into(),
        })
    }

    /// Boot the engine if it is not already healthy.
    ///
    /// Acquires the cross-process boot lock; a live holder fails cleanly.
    /// If the canary already passes, the engine is left alone. Otherwise the
    /// configured `launch_command` is spawned in its own process group and the
    /// canary is polled until healthy or the boot timeout elapses.
    pub async fn boot(&self) -> Result<(), LifecycleError> {
        match self.locks.acquire(BOOT_LOCK, BOOT_LOCK_TTL_MS)? {
            AcquireResult::HeldBy { pid } => return Err(LifecycleError::BootLockHeld { pid }),
            AcquireResult::Acquired => {}
        }
        let result = self.boot_inner().await;
        let _ = self.locks.release(BOOT_LOCK);
        result
    }

    async fn boot_inner(&self) -> Result<(), LifecycleError> {
        if self.canary().await {
            self.wedge.reset()?;
            return Err(LifecycleError::AlreadyRunning);
        }
        let cmd = self
            .config
            .launch_command
            .clone()
            .ok_or(LifecycleError::NoLaunchCommand)?;

        let err_buf: Arc<std::sync::Mutex<VecDeque<String>>> =
            Arc::new(std::sync::Mutex::new(VecDeque::new()));
        let mut c = Command::new("sh");
        c.arg("-c")
            .arg(&cmd)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        c.process_group(0);
        let mut child = c.spawn()?;
        {
            let stderr = child
                .stderr
                .take()
                .ok_or_else(|| LifecycleError::Stop("no stderr pipe".into()))?;
            let buf = err_buf.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let mut g = buf.lock().unwrap();
                    g.push_back(line);
                    if g.len() > STDERR_TAIL_LINES {
                        g.pop_front();
                    }
                }
            });
        }
        *self.child.lock().await = Some(child);

        let deadline = tokio::time::Instant::now() + self.boot_timeout;
        loop {
            if self.canary().await {
                self.wedge.reset()?;
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                let tail = err_buf
                    .lock()
                    .unwrap()
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n");
                let detail = if tail.is_empty() {
                    "no stderr captured".into()
                } else {
                    tail
                };
                let _ = self.kill_child().await;
                return Err(LifecycleError::BootTimeout {
                    secs: self.boot_timeout.as_secs(),
                    detail,
                });
            }
            tokio::time::sleep(BOOT_POLL).await;
        }
    }

    /// Probe the canary; on failure increment the wedge counter and report
    /// whether the threshold has been reached.
    pub async fn check_wedge(&self) -> Result<bool, LifecycleError> {
        if self.canary().await {
            self.wedge.reset()?;
            return Ok(false);
        }
        let n = self.wedge.incr()?;
        Ok(n >= WEDGE_THRESHOLD)
    }

    /// Stop and reboot a wedged engine. Serialized via the heal lock; a
    /// concurrent attempt fails cleanly on the lock.
    pub async fn heal(&self) -> Result<(), LifecycleError> {
        match self.locks.acquire(HEAL_LOCK, HEAL_LOCK_TTL_MS)? {
            AcquireResult::HeldBy { pid } => return Err(LifecycleError::HealLockHeld { pid }),
            AcquireResult::Acquired => {}
        }
        let result = self.heal_inner().await;
        let _ = self.locks.release(HEAL_LOCK);
        result
    }

    async fn heal_inner(&self) -> Result<(), LifecycleError> {
        self.wedge.reset()?;
        self.stop().await?;
        self.boot().await
    }

    /// Stop the engine: run the configured `stop_command` if present, else
    /// kill the spawned child's process group.
    pub async fn stop(&self) -> Result<(), LifecycleError> {
        if let Some(cmd) = &self.config.stop_command {
            let out = Command::new("sh").arg("-c").arg(cmd).output().await?;
            if !out.status.success() {
                return Err(LifecycleError::Stop(format!(
                    "stop command exited {}: {}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr).trim()
                )));
            }
            return Ok(());
        }
        self.kill_child().await
    }

    async fn kill_child(&self) -> Result<(), LifecycleError> {
        let Some(mut child) = self.child.lock().await.take() else {
            return Ok(());
        };
        if let Some(pid) = child.id() {
            #[cfg(unix)]
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
            let _ = child.wait().await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Ports;
    use axum::Json;
    use axum::Router;
    use axum::response::IntoResponse;
    use axum::routing::get;
    use serde_json::json;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TMP: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    fn tmp_state() -> StateDir {
        let n = TMP.fetch_add(1, Ordering::SeqCst);
        let p = std::env::temp_dir().join(format!("castor-lifecycle-{}-{}", std::process::id(), n));
        let s = StateDir::new(p);
        s.ensure().unwrap();
        s
    }

    /// Bounded poll for a marker file: the contract under test is that the
    /// command *runs*, not that it runs within a fixed window, so a
    /// synchronous `exists()` check is flaky under parallel-suite load.
    async fn wait_for_marker(path: &Path, timeout: Duration, interval: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if path.exists() {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(interval).await;
        }
    }

    fn test_config(state: &StateDir, launch: Option<&str>, stop: Option<&str>) -> Config {
        Config {
            model: Some("test-model".into()),
            base_url: None,
            api_key: None,
            engine_type: None,
            launch_command: launch.map(String::from),
            stop_command: stop.map(String::from),
            max_context: None,
            ports: Ports {
                engine: 0,
                status: 0,
                proxy: 0,
            },
            max_concurrent_tasks: 1,
            tool_prefix: String::new(),
            searxng_url: None,
            brave_api_key: None,
            openalex_email: None,
            openalex_api_key: None,
            boot_timeout_secs: 180,
            probe_budget: 4,
            state_dir: state.root().to_path_buf(),
        }
    }

    /// Mock engine: /v1/models returns 404 until `ready_after` requests have
    /// been served, then 200 listing `models`.
    async fn start_mock(models: Vec<String>, ready_after: u64) -> (String, u16) {
        let counter = Arc::new(AtomicU64::new(0));
        let app = Router::new().route(
            "/v1/models",
            get(move || {
                let counter = counter.clone();
                let models = models.clone();
                async move {
                    let n = counter.fetch_add(1, Ordering::SeqCst);
                    if n < ready_after {
                        (axum::http::StatusCode::NOT_FOUND, "unknown model").into_response()
                    } else {
                        Json(json!({
                            "data": models.iter().map(|id| json!({"id": id})).collect::<Vec<_>>()
                        }))
                        .into_response()
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://127.0.0.1:{}", addr.port()), addr.port())
    }

    #[tokio::test]
    async fn canary_healthy_when_configured_model_listed() {
        let state = tmp_state();
        let (_base, port) = start_mock(vec!["test-model".into()], 0).await;
        let mut cfg = test_config(&state, None, None);
        cfg.ports.engine = port;
        let lc = EngineLifecycle::new(&cfg, &state);
        assert!(lc.canary().await);

        // A healthy engine that does not list the configured id is not healthy.
        let mut cfg2 = test_config(&state, None, None);
        cfg2.model = Some("other-model".into());
        cfg2.ports.engine = port;
        let lc2 = EngineLifecycle::new(&cfg2, &state);
        assert!(!lc2.canary().await);
    }

    #[tokio::test]
    async fn boot_already_running_when_canary_healthy() {
        let state = tmp_state();
        let (_base, port) = start_mock(vec!["test-model".into()], 0).await;
        let mut cfg = test_config(&state, Some("sleep 30"), None);
        cfg.ports.engine = port;
        let lc = EngineLifecycle::new(&cfg, &state);
        let err = lc.boot().await.unwrap_err();
        assert!(matches!(err, LifecycleError::AlreadyRunning), "{err:?}");
    }

    #[tokio::test]
    async fn ensure_running_happy_path_when_already_healthy() {
        let state = tmp_state();
        let (_base, port) = start_mock(vec!["test-model".into()], 0).await;
        let mut cfg = test_config(&state, None, None);
        cfg.ports.engine = port;
        let lc = EngineLifecycle::new(&cfg, &state);
        assert!(lc.ensure_running().await.is_ok());
    }

    #[tokio::test]
    async fn ensure_running_fails_when_no_launch_command() {
        let state = tmp_state();
        let (_base, port) = start_mock(vec!["test-model".into()], u64::MAX).await;
        let mut cfg = test_config(&state, None, None);
        cfg.launch_command = None;
        cfg.ports.engine = port;
        let lc = EngineLifecycle::new(&cfg, &state);
        let err = lc.ensure_running().await.unwrap_err();
        assert!(matches!(err, LifecycleError::NoLaunchCommand));
    }

    #[tokio::test]
    async fn boot_spawns_when_canary_404s() {
        let state = tmp_state();
        let marker = state.root().join("launched.marker");
        let (_base, port) = start_mock(vec!["test-model".into()], u64::MAX).await;
        let mut cfg = test_config(&state, Some(&format!("touch {}", marker.display())), None);
        cfg.ports.engine = port;
        let lc = EngineLifecycle::new(&cfg, &state).with_boot_timeout(Duration::from_millis(2000));
        let err = lc.boot().await.unwrap_err();
        match &err {
            LifecycleError::BootTimeout { detail, .. } => {
                assert!(!detail.is_empty(), "expected captured stderr or a note");
            }
            other => panic!("expected BootTimeout, got {other:?}"),
        }
        assert!(
            wait_for_marker(&marker, Duration::from_secs(10), Duration::from_millis(50)).await,
            "launch command should have run"
        );
    }

    #[tokio::test]
    async fn wedge_threshold_triggers_heal() {
        let state = tmp_state();
        let stop_marker = state.root().join("stopped.marker");
        let (_base, port) = start_mock(vec!["test-model".into()], u64::MAX).await;
        let mut cfg = test_config(
            &state,
            Some("sleep 30"),
            Some(&format!("touch {}", stop_marker.display())),
        );
        cfg.ports.engine = port;
        let lc = EngineLifecycle::new(&cfg, &state).with_boot_timeout(Duration::from_millis(500));

        assert!(!lc.check_wedge().await.unwrap());
        assert!(!lc.check_wedge().await.unwrap());
        assert!(lc.check_wedge().await.unwrap());

        let err = lc.heal().await.unwrap_err();
        match &err {
            LifecycleError::BootTimeout { .. } => {}
            other => panic!("expected BootTimeout from heal, got {other:?}"),
        }
        assert!(
            wait_for_marker(
                &stop_marker,
                Duration::from_secs(10),
                Duration::from_millis(50)
            )
            .await,
            "heal should have run stop_command"
        );
        assert_eq!(lc.wedge.read(), 0, "heal should reset the wedge counter");
    }

    #[tokio::test]
    async fn concurrent_heal_fails_on_lock() {
        let state = tmp_state();
        let stop_marker = state.root().join("stopped2.marker");
        let (_base, port) = start_mock(vec!["test-model".into()], u64::MAX).await;
        let mut cfg = test_config(
            &state,
            Some("sleep 30"),
            Some(&format!("sleep 0.2; touch {}", stop_marker.display())),
        );
        cfg.ports.engine = port;
        let lc = Arc::new(
            EngineLifecycle::new(&cfg, &state).with_boot_timeout(Duration::from_millis(800)),
        );

        let a = lc.clone();
        let ta = tokio::spawn(async move { a.heal().await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        let b = lc.clone();
        let tb = tokio::spawn(async move { b.heal().await });

        let ra = ta.await.unwrap();
        let rb = tb.await.unwrap();

        let (first, second) = if matches!(ra, Err(LifecycleError::HealLockHeld { .. })) {
            (rb, ra)
        } else {
            (ra, rb)
        };
        match &first {
            Err(LifecycleError::BootTimeout { .. }) => {}
            other => panic!("expected BootTimeout from the lock holder, got {other:?}"),
        }
        match &second {
            Err(LifecycleError::HealLockHeld { pid }) => {
                assert_eq!(*pid, std::process::id());
            }
            other => panic!("expected HealLockHeld from the concurrent attempt, got {other:?}"),
        }
        assert!(stop_marker.exists());
    }

    #[tokio::test]
    async fn stop_runs_stop_command() {
        let state = tmp_state();
        let marker = state.root().join("stop.marker");
        let cfg = test_config(&state, None, Some(&format!("touch {}", marker.display())));
        let lc = EngineLifecycle::new(&cfg, &state);
        lc.stop().await.unwrap();
        assert!(marker.exists());
    }

    #[tokio::test]
    async fn stale_boot_lock_reclaimed() {
        let state = tmp_state();
        let (_base, port) = start_mock(vec!["test-model".into()], u64::MAX).await;
        let mut cfg = test_config(&state, Some("sleep 30"), None);
        cfg.ports.engine = port;

        // Backdate a foreign boot lock well beyond its TTL.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let lock_path = state.locks().join("engine_boot.lock");
        std::fs::write(
            lock_path,
            json!({"pid": 999999, "held_at": now - 500_000, "ttl": BOOT_LOCK_TTL_MS}).to_string(),
        )
        .unwrap();

        let lc = EngineLifecycle::new(&cfg, &state).with_boot_timeout(Duration::from_millis(500));
        // The stale foreign lock is reclaimed (renamed to a tombstone) and
        // re-acquired by us, so boot proceeds to the timeout rather than
        // failing with BootLockHeld.
        let err = lc.boot().await.unwrap_err();
        assert!(matches!(err, LifecycleError::BootTimeout { .. }), "{err:?}");
        assert!(
            state.locks().join("engine_boot.lock.tombstone").exists(),
            "stale lock should have been renamed to a tombstone"
        );
    }

    #[tokio::test]
    async fn stop_kills_process_group_when_no_stop_command() {
        let state = tmp_state();
        let pidfile = state.root().join("engine.pid");
        // Engine becomes healthy after 2 canary requests; the launch command
        // records its own pid so we can verify the group was killed.
        let (_base, port) = start_mock(vec!["test-model".into()], 2).await;
        let mut cfg = test_config(
            &state,
            Some(&format!("echo $$ > {}; sleep 30", pidfile.display())),
            None,
        );
        cfg.ports.engine = port;
        let lc = EngineLifecycle::new(&cfg, &state).with_boot_timeout(Duration::from_secs(5));
        lc.boot().await.unwrap();

        let mut pid: Option<u32> = None;
        for _ in 0..50 {
            if let Ok(s) = std::fs::read_to_string(&pidfile)
                && let Ok(p) = s.trim().parse()
            {
                pid = Some(p);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let pid = pid.expect("pidfile should be written by the launch command");
        assert!(
            Path::new(&format!("/proc/{pid}")).exists(),
            "child should be alive"
        );

        lc.stop().await.unwrap();
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "group should be dead"
        );
    }
}
