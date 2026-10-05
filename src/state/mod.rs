//! Persistent state layout and the lockfile / wedge-counter primitives.
//!
//! This is a faithful port of the `mcp-castor` lifecycle-lock semantics
//! (`server_lifecycle.js`): O_EXCL lockfiles with TTL-based stale recovery
//! via rename-to-tombstone, PID-ownership-checked release, and an atomic
//! (write-temp-then-rename) wedge counter.
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

use thiserror::Error;

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
            self.evo(),
        ] {
            fs::create_dir_all(&p)?;
        }
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
        fs::create_dir_all(&self.dir)?;
        let path = self.path(name);
        let pid = std::process::id();
        let now = now_millis();

        // 1. Fresh O_EXCL.
        if let Ok(mut f) = OpenOptions::new().create_new(true).write(true).open(&path) {
            write_lock(&mut f, pid, now, ttl_ms)?;
            return Ok(AcquireResult::Acquired);
        }

        // 2. Exists: reject if still within TTL.
        if let Some(info) = read_lock(&path)?
            && now.saturating_sub(info.held_at) < ttl_ms
        {
            return Ok(AcquireResult::HeldBy { pid: info.pid });
        }

        // 3. Stale (or unreadable): re-verify, rename to tombstone, O_EXCL.
        if let Some(info) = read_lock(&path)?
            && now.saturating_sub(info.held_at) < ttl_ms
        {
            return Ok(AcquireResult::HeldBy { pid: info.pid });
        }
        let fname = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| format!("{name}.lock"));
        let tombstone = self.dir.join(format!("{fname}.tombstone"));
        let _ = fs::rename(&path, &tombstone);

        match OpenOptions::new().create_new(true).write(true).open(&path) {
            Ok(mut f) => {
                write_lock(&mut f, pid, now, ttl_ms)?;
                Ok(AcquireResult::Acquired)
            }
            Err(_) => {
                // Raced loser: a competitor created a fresh lock in the gap.
                let holder = read_lock(&path)?.map(|i| i.pid).unwrap_or(0);
                Ok(AcquireResult::HeldBy { pid: holder })
            }
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
/// Cross-process atomicity is out of scope for this primitive (matching the
/// JS implementation); this mutex makes concurrent in-process increments
/// lossless.
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
