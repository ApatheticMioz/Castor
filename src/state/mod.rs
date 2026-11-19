//! Persistent state layout and the lockfile / wedge-counter primitives.
//!
//! Provides filesystem-backed synchronization across processes: O_EXCL lockfiles
//! with TTL-based stale recovery via rename-to-tombstone, PID-ownership-checked
//! release, and an atomic (write-temp-then-rename) wedge counter.
//!
//! Layout under the state dir:
//! ```text
//! <state>/
//!   tasks/
//!     slots/
//!   sessions/
//!   telemetry/
//!   locks/
//!     <name>.lock          # {pid, held_at, ttl}
//!     <name>.lock.tombstone# inert, left in place
//!     <name>.count         # wedge counter (integer)
//!   evo/
//! ```

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use thiserror::Error;

static LOCK_TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// The castor state directory and its typed sub-paths.
#[derive(Debug, Clone)]
pub struct StateDir {
    root: PathBuf,
}

impl StateDir {
    /// Build from an effective [`crate::config::Config`] (honors `state_dir`).
    pub fn from_config(config: &crate::config::Config) -> Self {
        Self {
            root: config.state_dir.clone(),
        }
    }

    /// Build directly from a root path.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn tasks(&self) -> PathBuf {
        self.root.join("tasks")
    }

    pub fn task_slots(&self) -> PathBuf {
        self.root.join("tasks").join("slots")
    }

    pub fn sessions(&self) -> PathBuf {
        self.root.join("sessions")
    }

    pub fn telemetry(&self) -> PathBuf {
        self.root.join("telemetry")
    }

    pub fn locks(&self) -> PathBuf {
        self.root.join("locks")
    }

    pub fn ports(&self) -> PathBuf {
        self.root.join("ports")
    }

    pub fn evo(&self) -> PathBuf {
        self.root.join("evo")
    }

    /// Create the full directory layout on demand (idempotent).
    pub fn ensure(&self) -> io::Result<()> {
        for p in [
            self.tasks(),
            self.task_slots(),
            self.sessions(),
            self.telemetry(),
            self.locks(),
            self.ports(),
            self.evo(),
        ] {
            fs::create_dir_all(&p)?;
        }
        let _ = Locks::from_state(self).cleanup_expired_tombstones(now_millis());
        Ok(())
    }
}

/// Metadata stored inside a lockfile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockInfo {
    pub pid: u32,
    /// Unix epoch milliseconds at which the lock was (re)acquired.
    pub held_at: u64,
    /// TTL in milliseconds; a lock older than this is stale and reclaimable.
    pub ttl_ms: u64,
}

/// Outcome of an [`Locks::acquire`] attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcquireResult {
    /// We now hold the lock.
    Acquired,
    /// The lock is held by another (live) process.
    HeldBy { pid: u32 },
}

/// Error from [`Locks::release`].
#[derive(Debug, Error)]
pub enum ReleaseError {
    #[error("lock '{name}' is held by foreign pid {pid}")]
    Foreign { name: String, pid: u32 },
    #[error("lock '{name}' is not held")]
    NotHeld { name: String },
    #[error("io error releasing lock '{name}': {source}")]
    Io {
        name: String,
        #[source]
        source: io::Error,
    },
}

impl From<io::Error> for ReleaseError {
    fn from(e: io::Error) -> Self {
        ReleaseError::Io {
            name: String::new(),
            source: e,
        }
    }
}

impl PartialEq for ReleaseError {
    fn eq(&self, other: &Self) -> bool {
        use ReleaseError::*;
        match (self, other) {
            (Foreign { name: a, pid: pa }, Foreign { name: b, pid: pb }) => a == b && pa == pb,
            (NotHeld { name: a }, NotHeld { name: b }) => a == b,
            (Io { name: a, .. }, Io { name: b, .. }) => a == b,
            _ => false,
        }
    }
}

/// Exclusive lockfiles under `<state>/locks/`.
#[derive(Debug, Clone)]
pub struct Locks {
    dir: PathBuf,
}

impl Locks {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn from_state(state: &StateDir) -> Self {
        Self::new(state.locks())
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Path of the lockfile for `name`.
    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.lock"))
    }

    /// Atomically acquire the exclusive lock `name` with a TTL.
    ///
    /// - Fresh O_EXCL create wins immediately.
    /// - An existing lock younger than `ttl_ms` is rejected with its holder pid.
    /// - A stale lock is atomically renamed to `<name>.lock.tombstone` (inert)
    ///   and a fresh O_EXCL is attempted; a raced loser fails cleanly.
    pub fn acquire(&self, name: &str, ttl_ms: u64) -> io::Result<AcquireResult> {
        self.acquire_with(name, ttl_ms, false)
    }

    /// Acquire the exclusive lock `name`, additionally reclaiming any in-TTL
    /// lock whose recorded `pid` is no longer alive (including Linux zombies).
    ///
    /// Used by long-lived singleton daemons (`proxy`, `status`) so a forced
    /// restart after `SIGKILL` does not leave the service locked out until the
    /// full TTL expires.
    pub fn acquire_live(&self, name: &str, ttl_ms: u64) -> io::Result<AcquireResult> {
        self.acquire_with(name, ttl_ms, true)
    }

    fn acquire_with(
        &self,
        name: &str,
        ttl_ms: u64,
        verify_pid_alive: bool,
    ) -> io::Result<AcquireResult> {
        fs::create_dir_all(&self.dir)?;
        let path = self.path(name);
        let pid = std::process::id();
        let now = now_millis();

        // 1. Fresh atomic create (staging file + hard_link avoids an empty 0-byte window).
        if self.try_create_exclusive(name, &path, pid, now, ttl_ms)? {
            return Ok(AcquireResult::Acquired);
        }

        let is_held = |info: &LockInfo| {
            now.saturating_sub(info.held_at) < ttl_ms
                && (!verify_pid_alive || crate::task::semaphore::pid_alive(info.pid))
        };

        // 2. Exists: reject if still active.
        if let Some(info) = read_lock_stable(&path)
            && is_held(&info)
        {
            return Ok(AcquireResult::HeldBy { pid: info.pid });
        }

        // 3. Stale (or unreadable): re-verify, rename to tombstone, exclusive create.
        if let Some(info) = read_lock_stable(&path)
            && is_held(&info)
        {
            return Ok(AcquireResult::HeldBy { pid: info.pid });
        }
        let fname = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| format!("{name}.lock"));
        let tombstone = self.dir.join(format!("{fname}.tombstone"));
        if tombstone.exists() {
            let _ = fs::remove_file(&tombstone);
        }
        let _ = fs::rename(&path, &tombstone);

        if self.try_create_exclusive(name, &path, pid, now, ttl_ms)? {
            Ok(AcquireResult::Acquired)
        } else {
            // Raced loser: a competitor created a fresh lock in the gap.
            let holder = read_lock_stable(&path).map(|i| i.pid).unwrap_or(0);
            Ok(AcquireResult::HeldBy { pid: holder })
        }
    }

    fn try_create_exclusive(
        &self,
        name: &str,
        path: &Path,
        pid: u32,
        now: u64,
        ttl_ms: u64,
    ) -> io::Result<bool> {
        let seq = LOCK_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = self.dir.join(format!(".{name}.lock.tmp.{pid}.{seq}"));
        let mut f = OpenOptions::new().create_new(true).write(true).open(&tmp)?;
        if let Err(err) = write_lock(&mut f, pid, now, ttl_ms) {
            drop(f);
            let _ = fs::remove_file(&tmp);
            return Err(err);
        }
        drop(f);

        let link_res = fs::hard_link(&tmp, path);
        let _ = fs::remove_file(&tmp);
        match link_res {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(false),
            Err(e) if e.kind() == io::ErrorKind::Unsupported => {
                match OpenOptions::new().create_new(true).write(true).open(path) {
                    Ok(mut f) => {
                        write_lock(&mut f, pid, now, ttl_ms)?;
                        Ok(true)
                    }
                    Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(false),
                    Err(err) => Err(err),
                }
            }
            Err(e) => Err(e),
        }
    }

    /// Release the lock `name` only if owned by this process.
    ///
    /// Returns [`ReleaseError::Foreign`] if another pid holds it, or
    /// [`ReleaseError::NotHeld`] if there is no lockfile.
    pub fn release(&self, name: &str) -> Result<(), ReleaseError> {
        let path = self.path(name);
        match read_lock(&path)? {
            Some(info) if info.pid == std::process::id() => {
                fs::remove_file(&path)?;
                Ok(())
            }
            Some(info) => Err(ReleaseError::Foreign {
                name: name.to_string(),
                pid: info.pid,
            }),
            None => Err(ReleaseError::NotHeld {
                name: name.to_string(),
            }),
        }
    }

    /// Remove any tombstone files in the locks directory whose TTL has expired.
    pub fn cleanup_expired_tombstones(&self, now: u64) -> usize {
        let mut cleaned = 0;
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return 0;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".tombstone") {
                continue;
            }
            let p = entry.path();
            let is_expired = if let Ok(Some(info)) = read_lock(&p) {
                now.saturating_sub(info.held_at) > info.ttl_ms
            } else {
                true
            };
            if is_expired && fs::remove_file(&p).is_ok() {
                cleaned += 1;
            }
        }
        cleaned
    }
}

/// Read a lockfile into [`LockInfo`]; `None` if absent.
fn read_lock(path: &Path) -> io::Result<Option<LockInfo>> {
    let raw = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let v: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let pid = v.get("pid").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
    let held_at = v.get("held_at").and_then(|x| x.as_u64()).unwrap_or(0);
    let ttl_ms = v.get("ttl").and_then(|x| x.as_u64()).unwrap_or(0);
    Ok(Some(LockInfo {
        pid,
        held_at,
        ttl_ms,
    }))
}

/// Read a lockfile cleanly, tolerating transient empty/partial reads during
/// non-hardlink fallback creation and returning `None` for missing or
/// persistently unreadable files.
fn read_lock_stable(path: &Path) -> Option<LockInfo> {
    for attempt in 0..5 {
        match read_lock(path) {
            Ok(info) => return info,
            Err(_) if attempt < 4 => std::thread::sleep(std::time::Duration::from_millis(2)),
            Err(_) => return None,
        }
    }
    None
}

/// Serialize a lock payload to an open file.
fn write_lock(f: &mut File, pid: u32, held_at: u64, ttl_ms: u64) -> io::Result<()> {
    let json = serde_json::json!({ "pid": pid, "held_at": held_at, "ttl": ttl_ms }).to_string();
    f.write_all(json.as_bytes())?;
    f.flush()
}

/// Test helper: write a lockfile with explicit metadata.
#[cfg(test)]
fn write_lock_file(path: &Path, pid: u32, held_at: u64, ttl_ms: u64) -> io::Result<()> {
    let mut f = File::create(path)?;
    write_lock(&mut f, pid, held_at, ttl_ms)
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Process-local serialization for wedge-counter read-modify-write.
///
/// Ensures concurrent in-process increments are lossless. Cross-process
/// atomicity relies on atomic write-and-rename of the counter file.
static COUNTER_LOCK: Mutex<()> = Mutex::new(());

/// An atomic (write-temp-then-rename) counter at `<locks>/<name>.count`.
#[derive(Debug, Clone)]
pub struct WedgeCounter {
    path: PathBuf,
}

impl WedgeCounter {
    pub fn new(locks_dir: impl AsRef<Path>, name: &str) -> Self {
        Self {
            path: locks_dir.as_ref().join(format!("{name}.count")),
        }
    }

    pub fn from_state(state: &StateDir, name: &str) -> Self {
        Self::new(state.locks(), name)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Current count (0 if absent or unreadable).
    pub fn read(&self) -> u64 {
        fs::read_to_string(&self.path)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }

    /// Atomically increment and return the new value.
    pub fn incr(&self) -> io::Result<u64> {
        let _g = COUNTER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let next = self.read() + 1;
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let fname = self
            .path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "counter".to_string());
        let tmp = self
            .path
            .parent()
            .map(|p| p.join(format!("{fname}.tmp_{}", std::process::id())))
            .unwrap_or_else(|| self.path.clone());
        fs::write(&tmp, next.to_string())?;
        fs::rename(&tmp, &self.path)?;
        Ok(next)
    }

    /// Reset the counter to zero.
    pub fn reset(&self) -> io::Result<()> {
        let _g = COUNTER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&self.path, "0")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn tmp_dir() -> PathBuf {
        let n = TMP_COUNTER.fetch_add(1, Ordering::SeqCst);
        let p = std::env::temp_dir().join(format!("castor-state-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn state_dir_layout() {
        let root = tmp_dir();
        let s = StateDir::new(&root);
        s.ensure().unwrap();
        assert!(s.tasks().is_dir());
        assert!(s.task_slots().is_dir());
        assert!(s.sessions().is_dir());
        assert!(s.telemetry().is_dir());
        assert!(s.locks().is_dir());
        assert!(s.evo().is_dir());
        assert_eq!(s.task_slots(), root.join("tasks").join("slots"));
    }

    #[test]
    fn fresh_acquire() {
        let dir = tmp_dir();
        let locks = Locks::new(&dir);
        let res = locks.acquire("boot", 60_000).unwrap();
        assert_eq!(res, AcquireResult::Acquired);
        assert!(locks.path("boot").exists());
        let info = read_lock(&locks.path("boot")).unwrap().unwrap();
        assert_eq!(info.pid, std::process::id());
        assert_eq!(info.ttl_ms, 60_000);
    }

    #[test]
    fn in_ttl_reject_with_held_by() {
        let dir = tmp_dir();
        let locks = Locks::new(&dir);
        let now = now_millis();
        write_lock_file(&locks.path("x"), 424242, now, 60_000).unwrap();
        let res = locks.acquire("x", 60_000).unwrap();
        assert_eq!(res, AcquireResult::HeldBy { pid: 424242 });
        // original lock untouched
        let info = read_lock(&locks.path("x")).unwrap().unwrap();
        assert_eq!(info.pid, 424242);
    }

    #[test]
    fn acquire_live_reclaims_dead_pid_within_ttl() {
        let dir = tmp_dir();
        let locks = Locks::new(&dir);
        #[cfg(unix)]
        let mut child = std::process::Command::new("true").spawn().unwrap();
        #[cfg(windows)]
        let mut child = std::process::Command::new("cmd")
            .args(["/c", "exit", "0"])
            .spawn()
            .unwrap();
        let dead_pid = child.id();
        let _ = child.wait();

        let now = now_millis();
        write_lock_file(&locks.path("live_dead"), dead_pid, now, 60_000).unwrap();
        assert_eq!(
            locks.acquire_live("live_dead", 60_000).unwrap(),
            AcquireResult::Acquired
        );
        let info = read_lock(&locks.path("live_dead")).unwrap().unwrap();
        assert_eq!(info.pid, std::process::id());
    }

    #[test]
    fn acquire_live_respects_live_pid_within_ttl() {
        let dir = tmp_dir();
        let locks = Locks::new(&dir);
        assert_eq!(
            locks.acquire_live("live_alive", 60_000).unwrap(),
            AcquireResult::Acquired
        );
        assert_eq!(
            locks.acquire_live("live_alive", 60_000).unwrap(),
            AcquireResult::HeldBy {
                pid: std::process::id()
            }
        );
    }

    #[test]
    fn acquire_live_concurrent_contention_single_winner() {
        let dir = tmp_dir();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(16));
        let mut handles = Vec::new();
        for _ in 0..16 {
            let b = barrier.clone();
            let d = dir.clone();
            handles.push(std::thread::spawn(move || {
                let locks = Locks::new(&d);
                b.wait();
                locks.acquire_live("race_live", 60_000).unwrap()
            }));
        }
        let outcomes: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let winners = outcomes
            .iter()
            .filter(|r| matches!(r, AcquireResult::Acquired))
            .count();
        assert_eq!(winners, 1, "exactly one racer may acquire: {outcomes:?}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn acquire_live_reclaims_unreaped_linux_zombie() {
        let dir = tmp_dir();
        let locks = Locks::new(&dir);
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let zombie_pid = child.id();
        // Poll until the child enters State: Z (<defunct>) without waiting on it.
        let status_path = format!("/proc/{zombie_pid}/status");
        let mut became_zombie = false;
        for _ in 0..400 {
            if fs::read_to_string(&status_path)
                .unwrap_or_default()
                .lines()
                .any(|l| {
                    l.starts_with("State:")
                        && l.trim_start_matches("State:").trim_start().starts_with('Z')
                })
            {
                became_zombie = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            became_zombie,
            "child {zombie_pid} did not enter State: Z within 2s"
        );

        let now = now_millis();
        write_lock_file(&locks.path("zombie_live"), zombie_pid, now, 60_000).unwrap();
        assert_eq!(
            locks.acquire_live("zombie_live", 60_000).unwrap(),
            AcquireResult::Acquired
        );
        let _ = child.wait();
    }

    #[test]
    fn stale_reclaim() {
        let dir = tmp_dir();
        let locks = Locks::new(&dir);
        let now = now_millis();
        // backdate well beyond the 30s TTL
        write_lock_file(&locks.path("y"), 999999, now - 100_000, 30_000).unwrap();
        let res = locks.acquire("y", 30_000).unwrap();
        assert_eq!(res, AcquireResult::Acquired);
        let info = read_lock(&locks.path("y")).unwrap().unwrap();
        assert_eq!(info.pid, std::process::id());
    }

    #[test]
    fn tombstone_race() {
        let dir = tmp_dir();
        let locks = Locks::new(&dir);

        // Scenario A: a competitor renamed a stale lock to a tombstone and
        // created a fresh lock; we must fail cleanly without clobbering.
        let name = "race";
        let path = locks.path(name);
        let now = now_millis();
        write_lock_file(&path, 999999, now - 100_000, 30_000).unwrap();
        let tombstone = dir.join(format!("{name}.lock.tombstone"));
        fs::rename(&path, &tombstone).unwrap();
        write_lock_file(&path, 888888, now, 60_000).unwrap();
        let res = locks.acquire(name, 60_000).unwrap();
        assert_eq!(res, AcquireResult::HeldBy { pid: 888888 });
        // competitor's fresh lock is intact
        assert_eq!(read_lock(&path).unwrap().unwrap().pid, 888888);
        // tombstone is inert (still present, does not block)
        assert!(tombstone.exists());

        // Scenario B: a bare tombstone (no live lock) allows fresh acquire.
        let name2 = "bare";
        let t2 = dir.join(format!("{name2}.lock.tombstone"));
        fs::write(&t2, "stale").unwrap();
        let res2 = locks.acquire(name2, 60_000).unwrap();
        assert_eq!(res2, AcquireResult::Acquired);
    }

    #[test]
    fn foreign_pid_release_refused() {
        let dir = tmp_dir();
        let locks = Locks::new(&dir);
        let now = now_millis();
        write_lock_file(&locks.path("f"), 777777, now, 60_000).unwrap();
        let err = locks.release("f").unwrap_err();
        assert_eq!(
            err,
            ReleaseError::Foreign {
                name: "f".into(),
                pid: 777777
            }
        );
        assert!(locks.path("f").exists(), "foreign lock must be preserved");
    }

    #[test]
    fn owner_release_and_reacquire() {
        let dir = tmp_dir();
        let locks = Locks::new(&dir);
        assert_eq!(locks.acquire("o", 60_000).unwrap(), AcquireResult::Acquired);
        locks.release("o").unwrap();
        assert!(!locks.path("o").exists());
        // re-acquire after release
        assert_eq!(locks.acquire("o", 60_000).unwrap(), AcquireResult::Acquired);
        locks.release("o").unwrap();
    }

    #[test]
    fn release_not_held() {
        let dir = tmp_dir();
        let locks = Locks::new(&dir);
        let err = locks.release("ghost").unwrap_err();
        assert_eq!(
            err,
            ReleaseError::NotHeld {
                name: "ghost".into()
            }
        );
    }

    #[tokio::test]
    async fn wedge_counter_concurrent() {
        let dir = tmp_dir();
        let counter = WedgeCounter::new(&dir, "engine");
        let n = 8;
        let mut handles = Vec::new();
        for _ in 0..n {
            let c = counter.clone();
            handles.push(tokio::spawn(async move { c.incr().unwrap() }));
        }
        let mut max = 0;
        for h in handles {
            max = max.max(h.await.unwrap());
        }
        assert_eq!(max, n as u64);
        assert_eq!(counter.read(), n as u64);
        // no leftover temp file
        assert!(!dir.join("engine.count.tmp").exists());
    }

    #[test]
    fn wedge_counter_sequential() {
        let dir = tmp_dir();
        let counter = WedgeCounter::new(&dir, "w");
        assert_eq!(counter.read(), 0);
        assert_eq!(counter.incr().unwrap(), 1);
        assert_eq!(counter.incr().unwrap(), 2);
        assert_eq!(counter.read(), 2);
    }
}
