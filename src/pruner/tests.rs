
use super::*;
use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static TMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn tmp_dir() -> PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::SeqCst);
    let p = std::env::temp_dir().join(format!("castor-pruner-{}-{}", std::process::id(), n));
    let _ = fs::remove_dir_all(&p);
    fs::create_dir_all(&p).unwrap();
    p
}

fn make_state(root: &Path) {
    fs::create_dir_all(root.join("tasks").join("slots")).unwrap();
    fs::create_dir_all(root.join("sessions")).unwrap();
    fs::create_dir_all(root.join("telemetry")).unwrap();
    fs::create_dir_all(root.join("locks")).unwrap();
    fs::create_dir_all(root.join("evo")).unwrap();
}

fn set_mtime(path: &Path, t: SystemTime) {
    let times = std::fs::FileTimes::new().set_modified(t);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;
        if let Ok(f) = fs::OpenOptions::new()
            .access_mode(FILE_WRITE_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
        {
            let _ = f.set_times(times);
        }
    }
    #[cfg(not(windows))]
    {
        if let Ok(f) = fs::File::open(path) {
            let _ = f.set_times(times);
        }
    }
}

/// Create a session dir with an `events.jsonl` carrying a terminal event.
fn make_session(root: &Path, id: &str, mtime: SystemTime) -> PathBuf {
    let dir = root.join("sessions").join(id);
    fs::create_dir_all(&dir).unwrap();
    let events = dir.join("events.jsonl");
    let mut f = fs::File::create(&events).unwrap();
    writeln!(f, r#"{{"type":"session_start"}}"#).unwrap();
    writeln!(f, r#"{{"type":"session_end"}}"#).unwrap();
    f.flush().unwrap();
    drop(f);
    set_mtime(&events, mtime);
    set_mtime(&dir, mtime);
    dir
}

/// Create a session dir with NO terminal event (active/open).
fn make_active_session(root: &Path, id: &str, mtime: SystemTime) -> PathBuf {
    let dir = root.join("sessions").join(id);
    fs::create_dir_all(&dir).unwrap();
    let events = dir.join("events.jsonl");
    let mut f = fs::File::create(&events).unwrap();
    writeln!(f, r#"{{"type":"session_start"}}"#).unwrap();
    f.flush().unwrap();
    drop(f);
    set_mtime(&events, mtime);
    set_mtime(&dir, mtime);
    dir
}

/// Create a task file.
fn make_task(root: &Path, id: &str, status: &str, session_id: &str, mtime: SystemTime) -> PathBuf {
    let path = root.join("tasks").join(format!("task_{id}.json"));
    let json = serde_json::json!({
        "id": id,
        "status": status,
        "session_id": session_id,
        "prompt": "test",
        "cwd": "/",
        "heartbeat": 0,
        "turns_budget": 10,
        "created_at": 0,
    });
    fs::write(&path, serde_json::to_string_pretty(&json).unwrap()).unwrap();
    set_mtime(&path, mtime);
    path
}

/// Create a slot lease referencing a task.
fn make_slot(root: &Path, idx: usize, task_id: &str) {
    let path = root
        .join("tasks")
        .join("slots")
        .join(format!("slot_{idx}.json"));
    let json = serde_json::json!({
        "tenant": 999999,
        "task_id": task_id,
        "at": 0,
        "hb": 0,
    });
    fs::write(&path, serde_json::to_string(&json).unwrap()).unwrap();
}

/// A time 30 days before `now` (well past the 14-day retention).
fn old_time(now: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(now - 30 * 86_400_000)
}

/// A time 1 day before `now` (well within the 14-day retention).
fn new_time(now: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(now - 86_400_000)
}

fn policy_now(now: u64) -> PrunePolicy {
    PrunePolicy {
        now: Some(now),
        ..Default::default()
    }
}

#[test]
fn plan_lists_exactly_old_sessions() {
    let root = tmp_dir();
    make_state(&root);

    let now = 100 * 86_400_000; // 100 days in epoch ms
    let old = old_time(now);
    let new = new_time(now);

    let old1 = make_session(&root, "s_old1", old);
    let old2 = make_session(&root, "s_old2", old);
    let new1 = make_session(&root, "s_new1", new);
    let active = make_active_session(&root, "s_active", old);

    let mut p = plan(&root, &policy_now(now));

    // Exactly the two old sessions.
    assert_eq!(
        p.sessions.len(),
        2,
        "expected 2 old sessions, got {:?}",
        p.sessions
    );
    assert!(p.sessions.contains(&old1));
    assert!(p.sessions.contains(&old2));
    assert!(!p.sessions.contains(&new1));
    assert!(!p.sessions.contains(&active));

    // Apply: deletes old, preserves new + active.
    let n = apply(&root, &mut p).unwrap();
    assert_eq!(n, 2);
    assert!(!old1.exists());
    assert!(!old2.exists());
    assert!(new1.exists());
    assert!(active.exists());

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn second_apply_is_typed_refusal() {
    let root = tmp_dir();
    make_state(&root);

    let now = 100 * 86_400_000;
    let old = old_time(now);
    let s = make_session(&root, "s1", old);

    let mut p = plan(&root, &policy_now(now));
    assert_eq!(p.sessions.len(), 1);
    apply(&root, &mut p).unwrap();
    assert!(!s.exists());

    // Second apply of the same plan → typed refusal.
    let err = apply(&root, &mut p).unwrap_err();
    assert_eq!(err, PruneError::AlreadyApplied);

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn empty_state_gives_empty_plan() {
    let root = tmp_dir();
    make_state(&root);

    let now = 100 * 86_400_000;
    let mut p = plan(&root, &policy_now(now));
    assert!(p.is_empty());
    assert_eq!(p.sessions.len(), 0);
    assert_eq!(p.tasks.len(), 0);
    assert_eq!(p.telemetry.len(), 0);
    assert_eq!(p.evo.len(), 0);
    assert_eq!(p.scratch.len(), 0);

    let n = apply(&root, &mut p).unwrap();
    assert_eq!(n, 0);

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn live_slot_task_preserved() {
    let root = tmp_dir();
    make_state(&root);

    let now = 100 * 86_400_000;
    let old = old_time(now);
    let new = new_time(now);

    // Task referenced by a live slot → protected.
    let live_task = make_task(&root, "t_live", "executing", "sess_live", old);
    make_slot(&root, 0, "t_live");

    // Old done task, not referenced → candidate.
    let old_task = make_task(&root, "t_old", "completed", "sess_old", old);

    // New done task → not old enough.
    let new_task = make_task(&root, "t_new", "completed", "sess_new", new);

    let mut p = plan(&root, &policy_now(now));

    assert!(
        !p.tasks.contains(&live_task),
        "live-slot task must not be a candidate"
    );
    assert!(
        p.tasks.contains(&old_task),
        "old done task should be a candidate"
    );
    assert!(
        !p.tasks.contains(&new_task),
        "new task should not be a candidate"
    );

    let n = apply(&root, &mut p).unwrap();
    assert_eq!(n, 1);
    assert!(live_task.exists(), "live-slot task must survive");
    assert!(!old_task.exists());
    assert!(new_task.exists());

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn lockfile_task_protected() {
    let root = tmp_dir();
    make_state(&root);

    let now = 100 * 86_400_000;
    let old = old_time(now);

    // Lockfile referencing a task → protected.
    let lock_path = root.join("locks").join("worker.lock");
    let lock_json = serde_json::json!({
        "pid": 424242,
        "held_at": 0,
        "ttl": 60000,
        "task_id": "t_locked",
    });
    fs::write(&lock_path, serde_json::to_string(&lock_json).unwrap()).unwrap();

    let locked_task = make_task(&root, "t_locked", "completed", "sess_l", old);

    let p = plan(&root, &policy_now(now));
    assert!(
        !p.tasks.contains(&locked_task),
        "lockfile-referenced task must not be a candidate"
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn stale_plan_refused() {
    let root = tmp_dir();
    make_state(&root);

    let now = 100 * 86_400_000;
    let old = old_time(now);
    let s = make_session(&root, "s1", old);

    let mut p = plan(&root, &policy_now(now));
    assert_eq!(p.sessions.len(), 1);

    // Drift: add a new session after the plan was computed.
    let new = new_time(now);
    make_session(&root, "s2", new);

    // Fingerprint no longer matches → NotFromPlan.
    let err = apply(&root, &mut p).unwrap_err();
    assert_eq!(err, PruneError::NotFromPlan);
    assert!(s.exists(), "nothing should have been deleted");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn count_gate_evicts_oldest() {
    let root = tmp_dir();
    make_state(&root);

    let now = 100 * 86_400_000;
    // 5 sessions, all within age, but max_count = 3 → evict 2 oldest.
    // s0 is the oldest (now-5d), s4 the newest (now-1d).
    let mut paths = Vec::new();
    for i in 0..5 {
        let t = UNIX_EPOCH + Duration::from_millis(now - (5 - i) * 86_400_000);
        paths.push(make_session(&root, &format!("s{i}"), t));
    }

    let policy = PrunePolicy {
        max_count: 3,
        now: Some(now),
        ..Default::default()
    };
    let p = plan(&root, &policy);

    // 5 total, max 3 → evict 2 oldest (s0, s1).
    assert_eq!(
        p.sessions.len(),
        2,
        "expected 2 evicted, got {:?}",
        p.sessions
    );
    assert!(p.sessions.contains(&paths[0]));
    assert!(p.sessions.contains(&paths[1]));
    assert!(!p.sessions.contains(&paths[2]));
    assert!(!p.sessions.contains(&paths[3]));
    assert!(!p.sessions.contains(&paths[4]));

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn size_gate_evicts_oldest() {
    let root = tmp_dir();
    make_state(&root);

    let now = 100 * 86_400_000;
    // 3 sessions, each exactly 1 MB, max_size_mb = 2 → evict oldest until ≤ 2 MB.
    // s0 is the oldest (now-3d), s2 the newest (now-1d).
    let mut paths = Vec::new();
    for i in 0..3 {
        let t = UNIX_EPOCH + Duration::from_millis(now - (3 - i) * 86_400_000);
        let dir = root.join("sessions").join(format!("s{i}"));
        fs::create_dir_all(&dir).unwrap();
        let events = dir.join("events.jsonl");
        let mut f = fs::File::create(&events).unwrap();
        // Write a newline-terminated JSON line (so it parses as a
        // terminal event), then pad so the file is EXACTLY 1 MB.
        let line = format!("{}\n", r#"{"type":"session_end"}"#);
        f.write_all(line.as_bytes()).unwrap();
        let pad = 1024 * 1024 - line.len();
        f.write_all(&vec![b'x'; pad]).unwrap();
        f.flush().unwrap();
        drop(f);
        set_mtime(&events, t);
        set_mtime(&dir, t);
        paths.push(dir);
    }

    let policy = PrunePolicy {
        max_size_mb: 2,
        now: Some(now),
        ..Default::default()
    };
    let p = plan(&root, &policy);

    // 3 × 1 MB = 3 MB > 2 MB → evict oldest (s0) → 2 MB ≤ 2 MB.
    assert_eq!(
        p.sessions.len(),
        1,
        "expected 1 evicted, got {:?}",
        p.sessions
    );
    assert!(p.sessions.contains(&paths[0]));

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn evo_old_dir_pruned() {
    let root = tmp_dir();
    make_state(&root);

    let now = 100 * 86_400_000;
    let old = old_time(now);
    let new = new_time(now);

    let old_evo = root.join("evo").join("cand_old");
    fs::create_dir_all(&old_evo).unwrap();
    set_mtime(&old_evo, old);

    let new_evo = root.join("evo").join("cand_new");
    fs::create_dir_all(&new_evo).unwrap();
    set_mtime(&new_evo, new);

    let mut p = plan(&root, &policy_now(now));
    assert_eq!(p.evo.len(), 1);
    assert!(p.evo.contains(&old_evo));
    assert!(!p.evo.contains(&new_evo));

    let n = apply(&root, &mut p).unwrap();
    assert_eq!(n, 1);
    assert!(!old_evo.exists());
    assert!(new_evo.exists());

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn telemetry_old_pruned() {
    let root = tmp_dir();
    make_state(&root);

    let now = 100 * 86_400_000;
    let old = old_time(now);

    let events = root.join("telemetry").join("events.jsonl");
    fs::write(&events, r#"{"level":"INFO"}"#).unwrap();
    set_mtime(&events, old);

    let mut p = plan(&root, &policy_now(now));
    assert_eq!(p.telemetry.len(), 1);
    assert!(p.telemetry.contains(&events));

    let n = apply(&root, &mut p).unwrap();
    assert_eq!(n, 1);
    assert!(!events.exists());

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn scratch_old_item_pruned() {
    let root = tmp_dir();
    make_state(&root);

    let scratch_dir = root.join(".scratch");
    fs::create_dir_all(&scratch_dir).unwrap();

    let now = 100 * 86_400_000;
    // 5 days old (> 4-day TTL)
    let old = UNIX_EPOCH + Duration::from_millis(now - 5 * 86_400_000);
    // 1 day old (< 4-day TTL)
    let new = UNIX_EPOCH + Duration::from_millis(now - 86_400_000);

    let old_file = scratch_dir.join("old_tool_out.txt");
    fs::write(&old_file, "old output").unwrap();
    set_mtime(&old_file, old);

    let new_file = scratch_dir.join("new_tool_out.txt");
    fs::write(&new_file, "new output").unwrap();
    set_mtime(&new_file, new);

    let old_subdir = scratch_dir.join("old_tmp");
    fs::create_dir_all(&old_subdir).unwrap();
    set_mtime(&old_subdir, old);

    let mut p = plan(&root, &policy_now(now));
    assert_eq!(p.scratch.len(), 2);
    assert!(p.scratch.contains(&old_file));
    assert!(p.scratch.contains(&old_subdir));
    assert!(!p.scratch.contains(&new_file));

    let n = apply(&root, &mut p).unwrap();
    assert_eq!(n, 2);
    assert!(!old_file.exists());
    assert!(!old_subdir.exists());
    assert!(new_file.exists());

    // Also test prune_scratch_dir helper directly
    let another_old = scratch_dir.join("another_old.txt");
    fs::write(&another_old, "another").unwrap();
    set_mtime(&another_old, old);

    let count = prune_scratch_dir(&scratch_dir, DEFAULT_SCRATCH_TTL_MS, Some(now)).unwrap();
    assert_eq!(count, 1);
    assert!(!another_old.exists());
    assert!(new_file.exists());

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn tombstone_pruning_and_fresh_retention() {
    let root = tmp_dir();
    make_state(&root);
    let now = 10_000_000u64;

    // Old tombstone: held_at = 1_000_000, ttl = 60_000 (expired 8+ million ms ago)
    let locks_dir = root.join("locks");
    let old_tombstone = locks_dir.join("worker.lock.tombstone");
    let old_json = serde_json::json!({
        "pid": 1234,
        "held_at": 1_000_000,
        "ttl_ms": 60_000,
    });
    fs::write(&old_tombstone, serde_json::to_string(&old_json).unwrap()).unwrap();

    // Fresh tombstone: held_at = 9_990_000, ttl = 60_000 (expires at 10_050_000)
    let fresh_tombstone = locks_dir.join("fresh.lock.tombstone");
    let fresh_json = serde_json::json!({
        "pid": 5678,
        "held_at": 9_990_000,
        "ttl_ms": 60_000,
    });
    fs::write(
        &fresh_tombstone,
        serde_json::to_string(&fresh_json).unwrap(),
    )
    .unwrap();

    let mut p = plan(&root, &policy_now(now));
    assert!(
        p.tombstones.contains(&old_tombstone),
        "expired tombstone must be planned for deletion"
    );
    assert!(
        !p.tombstones.contains(&fresh_tombstone),
        "fresh tombstone must NOT be planned for deletion"
    );

    let n = apply(&root, &mut p).unwrap();
    assert!(n >= 1);
    assert!(!old_tombstone.exists(), "expired tombstone must be deleted");
    assert!(fresh_tombstone.exists(), "fresh tombstone must survive");

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn auto_prune_last_prune_lifecycle() {
    let root = tmp_dir();
    make_state(&root);
    let now = 10_000_000u64;

    // 1. Missing .last_prune -> needs auto prune
    assert!(needs_auto_prune(&root, now));

    // 2. Record last prune at now
    record_last_prune(&root, now).unwrap();
    assert!(!needs_auto_prune(&root, now));
    assert!(!needs_auto_prune(&root, now + 1000));
    assert!(!needs_auto_prune(&root, now + AUTO_PRUNE_INTERVAL_MS - 1));

    // 3. Older than 24h -> needs auto prune
    assert!(needs_auto_prune(&root, now + AUTO_PRUNE_INTERVAL_MS));
    assert!(needs_auto_prune(&root, now + AUTO_PRUNE_INTERVAL_MS + 5000));

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn auto_prune_background_pass() {
    let root = tmp_dir();
    make_state(&root);

    // Put an old tombstone in locks
    let locks_dir = root.join("locks");
    let old_tombstone = locks_dir.join("stale.lock.tombstone");
    let old_json = serde_json::json!({
        "pid": 1234,
        "held_at": 1000,
        "ttl_ms": 1000,
    });
    fs::write(&old_tombstone, serde_json::to_string(&old_json).unwrap()).unwrap();

    // Background pass triggers because .last_prune is missing
    let handle = maybe_auto_prune(&root);
    assert!(
        handle.is_some(),
        "thread must be spawned when prune is needed"
    );
    handle.unwrap().join().unwrap();

    // .last_prune should now exist
    assert!(root.join(LAST_PRUNE_FILE).exists());
    // Old tombstone should be pruned
    assert!(!old_tombstone.exists());

    // Subsequent call returns None immediately (since .last_prune is fresh)
    assert!(maybe_auto_prune(&root).is_none());

    let _ = fs::remove_dir_all(&root);
}
