
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering as Ord};

static TMP: AtomicUsize = AtomicUsize::new(0);

fn tmp_state() -> StateDir {
    let n = TMP.fetch_add(1, Ord::SeqCst);
    let p = std::env::temp_dir().join(format!("castor-sem-{}-{}", std::process::id(), n));
    let s = StateDir::new(p);
    s.ensure().unwrap();
    s
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// Path to the compiled castor binary (used for cross-process tests).
///
/// Walks `current_exe()` ancestors so both classic Cargo layouts
/// (`target/debug/deps/castor-<hash>`) and Cargo 1.100+ build-dir layouts
/// (`target/debug/build/castor/<hash>/deps/castor-<hash>`) resolve the
/// compiled `castor` executable in `target/<profile>/`.
fn castor_bin() -> PathBuf {
    let bin_name = if cfg!(windows) {
        "castor.exe"
    } else {
        "castor"
    };
    let exe = std::env::current_exe().unwrap();
    for ancestor in exe.ancestors().skip(1) {
        let candidate = ancestor.join(bin_name);
        if candidate.is_file() {
            return candidate;
        }
        let debug_candidate = ancestor.join("debug").join(bin_name);
        if debug_candidate.is_file() {
            return debug_candidate;
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("debug")
        .join(bin_name)
}

/// A defunct (zombie) worker must be treated as DEAD so it cannot hold a
/// slot (e.g. `slot_0.json`) and deadlock other tenants.
///
/// Spawns a trivial child, lets it exit, but does NOT reap it — the kernel
/// keeps it as a `<defunct>` zombie. On Linux `kill(pid, 0)` still returns
/// 0 for a zombie, so this exercises the `/proc/{pid}/status` `State: Z`
/// re-inspection in [`pid_alive`].
#[cfg(target_os = "linux")]
#[test]
fn zombie_pid_is_treated_dead() {
    // Deliberately do NOT call .wait() before the probe: an unreaped,
    // exited child stays a zombie.
    let mut child = std::process::Command::new("true")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn `true`");
    let pid = child.id();
    assert!(pid > 0);

    // Poll until /proc/{pid}/status reports State: Z (i.e. it has exited
    // and become a zombie). This is an independent confirmation that the
    // process is a zombie, so the assertion below truly tests pid_alive.
    let status_path = format!("/proc/{pid}/status");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let is_zombie = std::fs::read_to_string(&status_path)
            .ok()
            .and_then(|c| {
                c.lines()
                    .find(|l| l.starts_with("State:"))
                    .map(|l| l.trim_start_matches("State:").trim_start().starts_with('Z'))
            })
            .unwrap_or(false);
        if is_zombie {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!("child {pid} did not become a zombie within 5s");
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    // The fix: a zombie must be reported as not alive.
    assert!(
        !pid_alive(pid),
        "zombie pid {pid} must be treated as dead (it cannot release its slot)"
    );

    // Reap so nothing is left around; after reaping the pid is gone.
    let _ = child.wait();
    assert!(!pid_alive(pid), "reaped pid {pid} must be dead");
}

#[test]
fn pid_alive_reports_current_and_dead_accurately() {
    assert!(
        pid_alive(std::process::id()),
        "current process must be alive"
    );
    assert!(!pid_alive(0), "pid 0 must never be alive");
    #[cfg(windows)]
    {
        let mut c = std::process::Command::new("cmd")
            .args(["/c", "exit", "0"])
            .spawn()
            .unwrap();
        let pid = c.id();
        let _ = c.wait();
        assert!(
            !pid_alive(pid),
            "exited process on Windows must be dead (GetExitCodeProcess != STILL_ACTIVE)"
        );
    }
    #[cfg(unix)]
    {
        let mut c = std::process::Command::new("true").spawn().unwrap();
        let pid = c.id();
        let _ = c.wait();
        assert!(!pid_alive(pid), "reaped process must be dead");
    }
}

#[test]
fn first_acquire_is_immediate_and_single_lease() {
    let state = tmp_state();
    let sem = TaskSemaphore::new(&state, 1);
    let lease = rt().block_on(sem.acquire("t1"));
    assert_eq!(sem.list_slots().len(), 1);
    let r = sem.release(&lease);
    assert_eq!(r, ReleaseResult::Released);
    assert_eq!(sem.list_slots().len(), 0);
}

#[test]
fn second_acquire_blocks_until_release() {
    let state = tmp_state();
    let sem = TaskSemaphore::with_poll_interval(&state, 1, 50);
    let a = rt().block_on(sem.acquire("a"));
    // Spawn a second acquire on the same runtime; it should block.
    let sem2 = sem.clone();
    let handle = rt().block_on(async move {
        let f = sem2.acquire("b");
        tokio::time::timeout(Duration::from_millis(200), f).await
    });
    // Should still be pending (timed out) because `a` is held.
    assert!(handle.is_err(), "second acquire should be blocked");
    // Now release `a`; the next acquire should succeed quickly.
    let _ = sem.release(&a);
    let b = rt().block_on(sem.acquire("b"));
    assert_eq!(b.task_id, "b");
    let _ = sem.release(&b);
    assert_eq!(sem.list_slots().len(), 0);
}

#[test]
fn double_release_is_idempotent() {
    let state = tmp_state();
    let sem = TaskSemaphore::new(&state, 1);
    let lease = rt().block_on(sem.acquire("x"));
    let r1 = sem.release(&lease);
    assert_eq!(r1, ReleaseResult::Released);
    let r2 = sem.release(&lease);
    assert_eq!(r2, ReleaseResult::Released);
    assert_eq!(sem.list_slots().len(), 0);
}

#[test]
fn zero_leftover_leases_after_release() {
    let state = tmp_state();
    let sem = TaskSemaphore::new(&state, 2);
    let a = rt().block_on(sem.acquire("a"));
    let b = rt().block_on(sem.acquire("b"));
    assert_eq!(sem.list_slots().len(), 2);
    let _ = sem.release(&a);
    let _ = sem.release(&b);
    let mut count = 0;
    for e in fs::read_dir(state.task_slots()).unwrap().flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        if n.starts_with("slot_") && n.ends_with(".json") {
            count += 1;
        }
    }
    assert_eq!(count, 0);
}

#[test]
fn cross_process_slot_exclusion() {
    let state = tmp_state();
    let sem = TaskSemaphore::with_poll_interval(&state, 1, 100);
    let bin = castor_bin();
    assert!(bin.exists(), "castor binary not found at {bin:?}");

    // Spawn a child that acquires a slot, prints HELD, holds 3s, releases.
    let mut child = std::process::Command::new(&bin)
        .args(["__sem_child"])
        .arg(state.root().to_str().unwrap())
        .arg("child_task")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();

    // Wait for HELD.
    use std::io::BufRead;
    let mut line = String::new();
    {
        let stdout = child.stdout.take().unwrap();
        let mut reader = std::io::BufReader::new(stdout);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let n = reader.read_line(&mut line).unwrap();
            if n == 0 {
                panic!("child exited before printing HELD");
            }
            if line.contains("HELD") {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("timeout waiting for HELD");
            }
        }
    }

    // Parent should be blocked while child holds the slot.
    let sem2 = sem.clone();
    let blocked = rt().block_on(async move {
        let f = sem2.acquire("parent_task");
        tokio::time::timeout(Duration::from_millis(500), f).await
    });
    assert!(
        blocked.is_err(),
        "parent acquire should be blocked while child holds slot"
    );

    // Wait for child to exit (it releases after 3s).
    let status = child.wait().unwrap();
    assert!(status.success(), "child should exit cleanly");

    // Now parent should acquire.
    let lease = rt().block_on(sem.acquire("parent_task"));
    assert_eq!(lease.task_id, "parent_task");
    let _ = sem.release(&lease);
    assert_eq!(sem.list_slots().len(), 0);
}

#[test]
fn alien_tenant_blocked_then_unblocked() {
    let state = tmp_state();
    let sem = TaskSemaphore::with_poll_interval(&state, 1, 100);
    let bin = castor_bin();
    assert!(bin.exists(), "castor binary not found at {bin:?}");

    // Child holds a slot for 2s.
    let mut child = std::process::Command::new(&bin)
        .args(["__sem_child"])
        .arg(state.root().to_str().unwrap())
        .arg("alien_task")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();

    use std::io::BufRead;
    let mut line = String::new();
    {
        let stdout = child.stdout.take().unwrap();
        let mut reader = std::io::BufReader::new(stdout);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let n = reader.read_line(&mut line).unwrap();
            if n == 0 {
                panic!("child exited before HELD");
            }
            if line.contains("HELD") {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("timeout waiting for HELD");
            }
        }
    }

    // Parent is blocked (alien tenant active).
    let sem2 = sem.clone();
    let blocked = rt().block_on(async move {
        let f = sem2.acquire("parent");
        tokio::time::timeout(Duration::from_millis(500), f).await
    });
    assert!(blocked.is_err(), "parent should be blocked by alien tenant");

    // Wait for child to exit.
    let status = child.wait().unwrap();
    assert!(status.success());

    // Now parent can acquire.
    let lease = rt().block_on(sem.acquire("parent"));
    assert_eq!(lease.task_id, "parent");
    let _ = sem.release(&lease);
}

#[test]
fn fifo_order() {
    let state = tmp_state();
    let sem = TaskSemaphore::with_poll_interval(&state, 1, 50);
    let rt = rt();

    // Do everything inside a single block_on so spawned tasks have a
    // reactor context.
    let sem_inner = sem.clone();
    let (_first, second) = rt.block_on(async move {
        let sem1 = sem_inner.clone();
        let sem2 = sem_inner.clone();
        let h1 = tokio::spawn(async move { sem1.acquire("first").await });
        let h2 = tokio::spawn(async move { sem2.acquire("second").await });

        // First should complete quickly.
        let first = tokio::time::timeout(Duration::from_millis(500), h1)
            .await
            .expect("first acquire should complete")
            .unwrap();
        assert_eq!(first.task_id, "first");

        // Release first; second should now acquire.
        sem_inner.release(&first);
        let second = tokio::time::timeout(Duration::from_millis(2000), h2)
            .await
            .expect("second acquire should complete after first is released")
            .unwrap();
        (first, second)
    });

    assert_eq!(second.task_id, "second");
    let _ = sem.release(&second);
    assert_eq!(sem.list_slots().len(), 0);
}

#[test]
fn test_concurrency_4_agents_acquire_distinct_slots() {
    let state = tmp_state();
    let sem = TaskSemaphore::new(&state, 4);
    let rt = rt();

    let leases = rt.block_on(async {
        let mut handles = Vec::new();
        for i in 0..4 {
            let s = sem.clone();
            let task_id = format!("agent_{i}");
            handles.push(tokio::spawn(async move { s.acquire(&task_id).await }));
        }
        let mut out = Vec::new();
        for h in handles {
            out.push(
                tokio::time::timeout(Duration::from_millis(1000), h)
                    .await
                    .unwrap()
                    .unwrap(),
            );
        }
        out
    });

    assert_eq!(leases.len(), 4);
    let mut slot_indices: Vec<usize> = leases.iter().map(|l| l.index).collect();
    slot_indices.sort_unstable();
    assert_eq!(slot_indices, vec![0, 1, 2, 3]);

    assert_eq!(sem.list_slots().len(), 4);
    let status = sem.slot_status(std::process::id() as u32);
    assert!(status.is_fully_occupied);
    assert_eq!(status.total_capacity, 4);
    assert_eq!(status.slots.len(), 4);

    for l in &leases {
        assert_eq!(sem.release(l), ReleaseResult::Released);
    }
    assert_eq!(sem.list_slots().len(), 0);
}

#[test]
fn test_concurrency_3_agents_in_4_slots_leaves_headroom() {
    let state = tmp_state();
    let sem = TaskSemaphore::new(&state, 4);
    let rt = rt();

    let leases = rt.block_on(async {
        let mut out = Vec::new();
        for i in 0..3 {
            let task_id = format!("agent_{i}");
            out.push(sem.acquire(&task_id).await);
        }
        out
    });

    assert_eq!(leases.len(), 3);
    assert_eq!(sem.list_slots().len(), 3);
    let status = sem.slot_status(std::process::id() as u32);
    assert!(!status.is_fully_occupied);
    assert_eq!(status.total_capacity, 4);

    // Immediate 4th acquire succeeds without blocking
    let agent4 = rt.block_on(async {
        tokio::time::timeout(Duration::from_millis(200), sem.acquire("agent_3"))
            .await
            .expect("4th agent should acquire immediately")
    });
    assert_eq!(sem.list_slots().len(), 4);

    // Release slot 1 (agent_1)
    assert_eq!(sem.release(&leases[1]), ReleaseResult::Released);
    assert_eq!(sem.list_slots().len(), 3);

    // 5th acquire should immediately reclaim slot 1
    let agent5 = rt.block_on(async {
        tokio::time::timeout(Duration::from_millis(200), sem.acquire("agent_4"))
            .await
            .expect("5th agent should claim freed slot immediately")
    });
    assert_eq!(agent5.index, leases[1].index);
    assert_eq!(sem.list_slots().len(), 4);

    // Clean up
    sem.release(&leases[0]);
    sem.release(&leases[2]);
    sem.release(&agent4);
    sem.release(&agent5);
    assert_eq!(sem.list_slots().len(), 0);
}

#[test]
fn test_concurrency_10_agents_4_slots_bounded_fifo() {
    use std::sync::atomic::{AtomicUsize, Ordering as Ord};

    let state = tmp_state();
    let sem = TaskSemaphore::with_poll_interval(&state, 4, 20);
    let rt = rt();

    let active_count = Arc::new(AtomicUsize::new(0));
    let max_observed = Arc::new(AtomicUsize::new(0));
    let total_completed = Arc::new(AtomicUsize::new(0));

    rt.block_on(async {
        let mut handles = Vec::new();
        for i in 0..10 {
            let s = sem.clone();
            let active = active_count.clone();
            let max_obs = max_observed.clone();
            let done = total_completed.clone();
            let task_id = format!("worker_{i}");

            handles.push(tokio::spawn(async move {
                let lease = s.acquire(&task_id).await;
                let cur = active.fetch_add(1, Ord::SeqCst) + 1;
                max_obs.fetch_max(cur, Ord::SeqCst);

                // Concurrency invariant: active slots MUST never exceed capacity of 4
                assert!(cur <= 4, "active count {cur} exceeded max_slots 4!");

                // Simulate work (15ms)
                tokio::time::sleep(Duration::from_millis(15)).await;

                active.fetch_sub(1, Ord::SeqCst);
                let res = s.release(&lease);
                assert_eq!(res, ReleaseResult::Released);
                done.fetch_add(1, Ord::SeqCst);
            }));
        }

        for h in handles {
            tokio::time::timeout(Duration::from_secs(10), h)
                .await
                .expect("task timed out")
                .unwrap();
        }
    });

    assert_eq!(total_completed.load(Ord::SeqCst), 10);
    assert_eq!(max_observed.load(Ord::SeqCst), 4);
    assert_eq!(active_count.load(Ord::SeqCst), 0);
    assert_eq!(sem.list_slots().len(), 0);
}

#[test]
fn test_multi_process_4_slots_alien_exclusion() {
    let state = tmp_state();
    let sem = TaskSemaphore::with_poll_interval(&state, 4, 50);
    let bin = castor_bin();
    assert!(bin.exists(), "castor binary not found at {bin:?}");

    // Spawn 4 separate child processes each claiming 1 slot and holding it.
    // The generous hold (30s) ensures slow CI runners never reap a child
    // before the parent observes all 4 slots; the parent kills them at the
    // end regardless.
    let mut children = Vec::new();
    for i in 0..4 {
        let child = std::process::Command::new(&bin)
            .args(["__sem_child"])
            .arg(state.root().to_str().unwrap())
            .arg(format!("child_task_{i}"))
            .arg("--max-slots")
            .arg("4")
            .arg("--hold-secs")
            .arg("30")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        children.push(child);
    }

    use std::io::BufRead;
    // Wait for all 4 children to output "HELD"
    for child in &mut children {
        let stdout = child.stdout.take().unwrap();
        let mut reader = std::io::BufReader::new(stdout);
        let mut line = String::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let n = reader.read_line(&mut line).unwrap();
            if n == 0 {
                panic!("child exited before HELD");
            }
            if line.contains("HELD") {
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("timeout waiting for HELD");
            }
        }
    }

    // Poll until all 4 slots are visible (up to 5s). A child can claim its
    // slot and print HELD in any order; the parent's snapshot must not be
    // taken before every child's lease file is fully on disk.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let n = sem.list_slots().len();
        if n == 4 || std::time::Instant::now() > deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(
        sem.list_slots().len(),
        4,
        "all 4 child slot leases must be visible after polling"
    );

    // A 5th acquire from the parent process MUST block!
    let sem_clone = sem.clone();
    let blocked = rt().block_on(async move {
        let f = sem_clone.acquire("parent_task_5th");
        tokio::time::timeout(Duration::from_millis(300), f).await
    });
    assert!(
        blocked.is_err(),
        "5th acquire must be blocked when 4 slots are occupied"
    );

    // Kill the children: the 5th-acquire-blocks check above has proven the
    // alien-exclusion invariant. We kill (rather than wait for the full
    // 30s hold) so the test stays fast; a killed child's lease is then
    // reclaimable (dead tenant) and the parent can acquire.
    for mut child in children.drain(..) {
        let _ = child.kill();
        let _ = child.wait();
    }

    // The killed children's leases are now reclaimable (dead tenant), so a
    // parent acquire must succeed once the stale leases are cleaned up.
    let lease = rt().block_on(async {
        tokio::time::timeout(
            Duration::from_secs(5),
            sem.acquire("parent_task_after_drain"),
        )
        .await
        .expect("parent acquire should succeed after drain")
    });
    assert_eq!(lease.task_id, "parent_task_after_drain");
    assert_eq!(sem.release(&lease), ReleaseResult::Released);
    assert_eq!(sem.list_slots().len(), 0);
}

#[test]
fn test_multi_slot_crashed_tenant_reclaim() {
    let state = tmp_state();
    let sem = TaskSemaphore::new(&state, 4);

    // Artificially inject a dead lease at slot 1 and slot 3 (PID 99999999)
    let dead_lease_1 = Lease {
        tenant: 99999999,
        task_id: "crashed_1".into(),
        at: 1000,
        hb: 1000,
        context_tokens: 0,
    };
    let dead_lease_3 = Lease {
        tenant: 99999999,
        task_id: "crashed_3".into(),
        at: 1000,
        hb: 1000,
        context_tokens: 0,
    };
    fs::write(
        sem.slot_path(1),
        serde_json::to_string(&dead_lease_1).unwrap(),
    )
    .unwrap();
    fs::write(
        sem.slot_path(3),
        serde_json::to_string(&dead_lease_3).unwrap(),
    )
    .unwrap();

    // Clear reclaimable slots
    sem.clear_reclaimable();
    assert_eq!(sem.list_slots().len(), 0);

    // Now acquire all 4 slots cleanly
    let l0 = rt().block_on(sem.acquire("t0"));
    let l1 = rt().block_on(sem.acquire("t1"));
    let l2 = rt().block_on(sem.acquire("t2"));
    let l3 = rt().block_on(sem.acquire("t3"));
    assert_eq!(sem.list_slots().len(), 4);

    sem.release(&l0);
    sem.release(&l1);
    sem.release(&l2);
    sem.release(&l3);
    assert_eq!(sem.list_slots().len(), 0);
}

/// `lease_reclaimable_full` combines the dead-owner rule with the
/// terminal-task rule. A terminal task bound to a LIVE owner is the zombie
/// case the plain dead-owner check would miss; it must be reclaimable.
#[test]
fn lease_reclaimable_full_terminal_task_live_owner() {
    use crate::task::registry::{TaskRegistry, TaskStatus};
    let state = tmp_state();
    let reg = TaskRegistry::new(&state);
    let sem = TaskSemaphore::new(&state, 2);
    let me = std::process::id() as u32;

    let done = rt().block_on(reg.create("p", "/tmp", "s")).unwrap();
    let _ = rt()
        .block_on(reg.transition(&done, TaskStatus::Completed, None))
        .unwrap();

    let lease = Lease {
        tenant: me, // live owner
        task_id: done,
        at: 0,
        hb: 0,
        context_tokens: 0,
    };
    assert!(
        sem.lease_reclaimable_full(Some(&lease)),
        "a live owner whose task is terminal must be reclaimable"
    );
}

/// A LIVE owner whose task is still ACTIVE (or unknown) must not be
/// reclaimed, even though the plain dead-owner check would also say no.
#[test]
fn lease_reclaimable_full_active_task_live_owner_not_reclaimable() {
    use crate::task::registry::{TaskRegistry, TaskStatus};
    let state = tmp_state();
    let reg = TaskRegistry::new(&state);
    let sem = TaskSemaphore::new(&state, 2);
    let me = std::process::id() as u32;

    let active = rt().block_on(reg.create("p", "/tmp", "s")).unwrap();
    let _ = rt()
        .block_on(reg.update(&active, |r| r.pid = Some(me)))
        .unwrap();
    let _ = rt()
        .block_on(reg.transition(&active, TaskStatus::Executing, None))
        .unwrap();

    // Active task, live owner → NOT reclaimable.
    let lease = Lease {
        tenant: me,
        task_id: active.clone(),
        at: 0,
        hb: 0,
        context_tokens: 0,
    };
    assert!(
        !sem.lease_reclaimable_full(Some(&lease)),
        "a live owner with an active task must NOT be reclaimable"
    );

    // Unknown task id, live owner → NOT reclaimable (no terminal signal).
    let ghost = Lease {
        tenant: me,
        task_id: "task_ghost".into(),
        at: 0,
        hb: 0,
        context_tokens: 0,
    };
    assert!(
        !sem.lease_reclaimable_full(Some(&ghost)),
        "a live owner with an unknown task must NOT be reclaimable"
    );

    // Dead owner is always reclaimable, regardless of task state.
    let dead = Lease {
        tenant: 4_999_999, // almost certainly not alive
        task_id: active,
        at: 0,
        hb: 0,
        context_tokens: 0,
    };
    assert!(
        sem.lease_reclaimable_full(Some(&dead)),
        "a dead owner must be reclaimable regardless of task state"
    );
}

/// `bound_task_terminal` reports the on-disk terminal state of a lease's
/// bound task (`Some(true)` / `Some(false)` / `None`).
#[test]
fn bound_task_terminal_reads_disk_status() {
    use crate::task::registry::{TaskRegistry, TaskStatus};
    let state = tmp_state();
    let reg = TaskRegistry::new(&state);
    let sem = TaskSemaphore::new(&state, 2);

    let done = rt().block_on(reg.create("p", "/tmp", "s")).unwrap();
    let _ = rt()
        .block_on(reg.transition(&done, TaskStatus::Failed, Some("x".into())))
        .unwrap();
    assert_eq!(
        sem.bound_task_terminal(&Lease {
            tenant: 1,
            task_id: done,
            at: 0,
            hb: 0,
            context_tokens: 0,
        }),
        Some(true)
    );

    let active = rt().block_on(reg.create("p", "/tmp", "s")).unwrap();
    assert_eq!(
        sem.bound_task_terminal(&Lease {
            tenant: 1,
            task_id: active,
            at: 0,
            hb: 0,
            context_tokens: 0,
        }),
        Some(false)
    );

    assert_eq!(
        sem.bound_task_terminal(&Lease {
            tenant: 1,
            task_id: "task_ghost".into(),
            at: 0,
            hb: 0,
            context_tokens: 0,
        }),
        None
    );
}

// ------------------------------------------------------------------
// Token-budget tests
// ------------------------------------------------------------------

/// A lease acquired via the plain `acquire` path must have
/// `context_tokens == 0` (default).
#[test]
fn context_tokens_default_zero_on_acquire() {
    let state = tmp_state();
    let sem = TaskSemaphore::new(&state, 1);
    let sem_c = sem.clone();
    let lease = rt().block_on(async move { sem_c.acquire("t1").await });
    let on_disk = sem.read_lease(lease.index).unwrap();
    assert_eq!(on_disk.context_tokens, 0);
    let _ = sem.release(&lease);
}

/// A `Lease` JSON written before `context_tokens` existed must
/// deserialize with `context_tokens == 0` (backward compat).
#[test]
fn context_tokens_backward_compat_deserialization() {
    let json = r#"{"tenant":123,"task_id":"t","at":1000,"hb":1000}"#;
    let lease: Lease = serde_json::from_str(json).unwrap();
    assert_eq!(lease.context_tokens, 0);
    assert_eq!(lease.task_id, "t");
}

/// `update_context_tokens` must persist the new value to disk.
#[test]
fn update_context_tokens_persists_to_disk() {
    let state = tmp_state();
    let sem = TaskSemaphore::new(&state, 2);
    let sem_c = sem.clone();
    let lease = rt().block_on(async move { sem_c.acquire("task_a").await });
    assert_eq!(sem.read_lease(lease.index).unwrap().context_tokens, 0);

    sem.update_context_tokens("task_a", 42);
    let updated = sem.read_lease(lease.index).unwrap();
    assert_eq!(updated.context_tokens, 42);

    // A non-matching task_id must NOT modify this lease.
    sem.update_context_tokens("other_task", 99);
    let unchanged = sem.read_lease(lease.index).unwrap();
    assert_eq!(
        unchanged.context_tokens, 42,
        "non-matching task_id must not overwrite"
    );

    let _ = sem.release(&lease);
}

/// With `pool_capacity_tokens = 100`:
/// - First acquire (no live lease) → gate bypassed, claim succeeds.
/// - Second acquire with `initial_tokens = 50`:
///   committed = 50; new_total = 50 + 50 = 100 <= 100 → admitted.
///   Bump it to 80: committed = 80; new_total = 80 + 50 = 130 > 100
///   → must be BLOCKED (times out).
#[test]
fn with_token_budget_blocks_when_over_capacity() {
    let state = tmp_state();
    let sem = TaskSemaphore::with_poll_interval(&state, 4, 50).with_token_budget(100);
    let rt = rt();

    // First acquire: no live leases → gate bypassed.
    let sem_c1 = sem.clone();
    let l1 = rt.block_on(async move {
        tokio::time::timeout(
            Duration::from_millis(500),
            sem_c1.acquire_with_tokens("task_a", 50),
        )
        .await
        .expect("first acquire should succeed (no live leases yet)")
    });
    assert_eq!(sem.list_slots().len(), 1);

    // Second acquire: 1 live lease (context_tokens=50).
    // committed = 50
    // new_total = 50 + 50 = 100 <= 100 → admitted
    let sem_c2 = sem.clone();
    let l2 = rt.block_on(async move {
        tokio::time::timeout(
            Duration::from_millis(500),
            sem_c2.acquire_with_tokens("task_b", 50),
        )
        .await
        .expect("second acquire should be admitted: 100 <= 100")
    });
    assert_eq!(sem.list_slots().len(), 2);

    // Grow `a` to 80: committed = 80 + 50 = 130, so a third claim of even
    // 1 token overcommits (130 + 1 > 100) → BLOCKED.
    sem.update_context_tokens("task_a", 80);
    let sem_c3 = sem.clone();
    let blocked = rt.block_on(async move {
        tokio::time::timeout(
            Duration::from_millis(200),
            sem_c3.acquire_with_tokens("task_c", 1),
        )
        .await
    });
    assert!(
        blocked.is_err(),
        "third acquire must be blocked: 130 + 1 > 100 (pool_capacity)"
    );

    let _ = sem.release(&l1);
    let _ = sem.release(&l2);
    assert_eq!(sem.list_slots().len(), 0);
}

/// With `pool_capacity_tokens = 100`:
/// - First acquire with `initial_tokens = 50` → 50 committed.
/// - Second acquire with `initial_tokens = 20`:
///   committed = 50; new_total = 50 + 20 = 70 <= 100 → must SUCCEED.
#[test]
fn with_token_budget_allows_when_under_capacity() {
    let state = tmp_state();
    let sem = TaskSemaphore::with_poll_interval(&state, 4, 50).with_token_budget(100);
    let rt = rt();

    let sem_c1 = sem.clone();
    let l1 = rt.block_on(async move {
        tokio::time::timeout(
            Duration::from_millis(500),
            sem_c1.acquire_with_tokens("task_a", 50),
        )
        .await
        .expect("first acquire should succeed")
    });
    assert_eq!(sem.list_slots().len(), 1);

    // 50 committed + 20 (initial) = 70 <= 100 → OK
    let sem_c2 = sem.clone();
    let l2 = rt.block_on(async move {
        tokio::time::timeout(
            Duration::from_millis(500),
            sem_c2.acquire_with_tokens("task_b", 20),
        )
        .await
        .expect("second acquire should succeed: 70 <= 100")
    });
    assert_eq!(sem.list_slots().len(), 2);

    // Verify the persisted context_tokens values.
    let slot_a = sem.read_lease(l1.index).unwrap();
    let slot_b = sem.read_lease(l2.index).unwrap();
    assert_eq!(slot_a.context_tokens, 50);
    assert_eq!(slot_b.context_tokens, 20);

    sem.release(&l1);
    sem.release(&l2);
    assert_eq!(sem.list_slots().len(), 0);
}

/// When `pool_capacity_tokens == 0` (default), the token-budget gate is
/// disabled and `acquire_with_tokens` behaves identically to `acquire`.
#[test]
fn zero_pool_capacity_disables_token_gate() {
    let state = tmp_state();
    // Default: pool_capacity_tokens = 0 → gate disabled.
    let sem = TaskSemaphore::with_poll_interval(&state, 4, 50);
    let rt = rt();

    // Acquire with a large initial_tokens value — should succeed
    // because the gate is disabled (pool_capacity == 0).
    let sem_c1 = sem.clone();
    let l1 = rt.block_on(async move {
        tokio::time::timeout(
            Duration::from_millis(500),
            sem_c1.acquire_with_tokens("task_a", 999_999),
        )
        .await
        .expect("should succeed when gate is disabled")
    });
    let sem_c2 = sem.clone();
    let l2 = rt.block_on(async move {
        tokio::time::timeout(
            Duration::from_millis(500),
            sem_c2.acquire_with_tokens("task_b", 999_999),
        )
        .await
        .expect("second should also succeed when gate is disabled")
    });
    assert_eq!(sem.list_slots().len(), 2);

    sem.release(&l1);
    sem.release(&l2);
}

/// `update_context_tokens` followed by a second acquire must reflect the
/// updated token cost in the budget gate.
#[test]
fn update_context_tokens_affects_budget_gate() {
    let state = tmp_state();
    let sem = TaskSemaphore::with_poll_interval(&state, 4, 50).with_token_budget(100);
    let rt = rt();

    // First acquire with 20 initial tokens → committed = 20.
    let sem_c1 = sem.clone();
    let l1 = rt.block_on(async move {
        tokio::time::timeout(
            Duration::from_millis(500),
            sem_c1.acquire_with_tokens("task_a", 20),
        )
        .await
        .expect("first acquire")
    });

    // Bump context_tokens to 90: a new claim of 20 would overcommit
    // (90 + 20 = 110 > 100), but 10 still fits (90 + 10 = 100 <= 100).
    sem.update_context_tokens("task_a", 90);

    // Second acquire with 20 initial tokens:
    // new_total = 90 + 20 = 110 > 100 → BLOCKED.
    let sem_c2 = sem.clone();
    let blocked = rt.block_on(async move {
        tokio::time::timeout(
            Duration::from_millis(200),
            sem_c2.acquire_with_tokens("task_b", 20),
        )
        .await
    });
    assert!(
        blocked.is_err(),
        "second acquire must be blocked after context_tokens update: 110 > 100"
    );

    // A claim that still fits under the (updated) committed total is
    // admitted — proving the gate reflects the new footprint.
    let sem_c3 = sem.clone();
    let ok = rt.block_on(async move {
        tokio::time::timeout(
            Duration::from_millis(500),
            sem_c3.acquire_with_tokens("task_c", 10),
        )
        .await
    });
    assert!(
        ok.is_ok(),
        "10-token claim must fit under the 90 committed (100 <= 100)"
    );

    let _ = sem.release(&l1);
    if let Ok(c) = ok {
        let _ = sem.release(&c);
    }
}

// ------------------------------------------------------------------
// `should_yield` (pool-overcommit yield gate)
// ------------------------------------------------------------------

/// A solo worker (only one active lease) never yields, even when its own
/// context overcommits an otherwise-empty budget.
#[test]
fn should_yield_solo_never_yields() {
    let state = tmp_state();
    let sem = TaskSemaphore::with_poll_interval(&state, 1, 50).with_token_budget(50);
    // Admitted (empty pool, 10 <= 50); then grown past the budget.
    let a = rt().block_on(sem.acquire_with_tokens("a", 10));
    sem.update_context_tokens("a", 100);
    // Over budget (100 > 50) but a solo holder keeps its slot.
    assert!(!sem.should_yield(&a));
}

/// Under pool overcommit, the task with the higher KV cache footprint is
/// prioritized to complete and free its memory without KV cache thrashing.
/// The lighter task yields/pauses.
#[test]
fn should_yield_prioritizes_higher_context_task_when_overcommitted() {
    let state = tmp_state();
    let sem = TaskSemaphore::with_poll_interval(&state, 2, 50).with_token_budget(100);
    // Both admitted up front (40 + 40 = 80 <= 100).
    let a = rt().block_on(sem.acquire_with_tokens("a", 40));
    let b = rt().block_on(sem.acquire_with_tokens("b", 40));
    // Grow lease `a` to 80 → committed = 80 + 40 = 120 > 90 (soft limit).
    sem.update_context_tokens("a", 80);
    // `a` has higher context (80 vs 40), so `a` does NOT yield; `b` yields to let `a` finish.
    assert!(!sem.should_yield(&a));
    assert!(sem.should_yield(&b));
}

/// A younger lease with a low cache-hit ratio but a pool that is NOT
/// overcommitted does NOT yield: overcommit is the only trigger, so the
/// pool's own budget — not the empirical cache ratio — drives the decision.
#[test]
fn should_yield_younger_healthy_pool_does_not_yield() {
    let state = tmp_state();
    let sem = TaskSemaphore::with_poll_interval(&state, 2, 50).with_token_budget(1000);
    let _a = rt().block_on(sem.acquire_with_tokens("a", 10));
    let b = rt().block_on(sem.acquire_with_tokens("b", 50)); // younger
    // committed = 60 <= 900 → no overcommit → no yield.
    assert!(!sem.should_yield(&b));
}

/// When a younger task grows into the heavier session, it is prioritized
/// to finish and the lighter task yields.
#[test]
fn should_yield_younger_pool_overcommit_yields() {
    let state = tmp_state();
    let sem = TaskSemaphore::with_poll_interval(&state, 2, 50).with_token_budget(50);
    let a = rt().block_on(sem.acquire_with_tokens("a", 5));
    let b = rt().block_on(sem.acquire_with_tokens("b", 5)); // younger
    // Grow `b`'s context so committed = 5 + 100 = 105 > 45 (soft limit).
    sem.update_context_tokens("b", 100);
    // `b` is the heavy task (100 > 5), so `b` does not yield and `a` yields.
    assert!(!sem.should_yield(&b));
    assert!(sem.should_yield(&a));
}

/// When context sizes are tied under overcommit, senior task wins tie-breaker.
#[test]
fn should_yield_tie_breaker_senior_task_wins() {
    let state = tmp_state();
    let sem = TaskSemaphore::with_poll_interval(&state, 2, 50).with_token_budget(100);
    let a = rt().block_on(sem.acquire_with_tokens("a", 50));
    std::thread::sleep(Duration::from_millis(5));
    let b = rt().block_on(sem.acquire_with_tokens("b", 50));
    // committed = 100 > 90 (soft limit). Contexts are equal (50 == 50).
    // `a` is senior (acquired first), so `a` does not yield and `b` yields.
    assert!(!sem.should_yield(&a));
    assert!(sem.should_yield(&b));
}

// ------------------------------------------------------------------
// Compare-and-delete slot reaping (ABA-safe)
// ------------------------------------------------------------------

/// The re-read-then-unlink must DELETE a slot whose `(tenant, task_id, at)`
/// identity is unchanged between the reclamation evaluation and the
/// unlink — the normal zombie-slot path.
#[test]
fn reap_if_match_deletes_when_lease_unchanged() {
    let state = tmp_state();
    let sem = TaskSemaphore::new(&state, 2);
    let path = sem.slot_path(0);
    let evaluated = Lease {
        tenant: 4_999_999, // dead owner → reclaimable
        task_id: "t".into(),
        at: 1,
        hb: 1,
        context_tokens: 0,
    };
    fs::write(&path, serde_json::to_string(&evaluated).unwrap()).unwrap();
    assert!(
        sem.reap_if_match(&path, Some(&evaluated)),
        "an unchanged dead-owner lease must be deleted"
    );
    assert!(!path.exists());
}

/// If the slot is **claimed or updated** (different `(tenant, task_id, at)`
/// identity) after the reclamation evaluation, the compare must fail and
/// the file must be left intact — a plain unlink would destroy the new
/// lease (ABA).
#[test]
fn reap_if_match_keeps_when_lease_changed() {
    let state = tmp_state();
    let sem = TaskSemaphore::new(&state, 2);
    let path = sem.slot_path(0);
    // The reaper evaluated an old dead-owner lease…
    let old = Lease {
        tenant: 4_999_999,
        task_id: "t".into(),
        at: 1,
        hb: 1,
        context_tokens: 0,
    };
    fs::write(&path, serde_json::to_string(&old).unwrap()).unwrap();
    // …but the slot has since been claimed/updated by a different tenant
    // (or re-stamped): the re-read no longer matches `old`.
    let new = Lease {
        tenant: 1,
        task_id: "t2".into(),
        at: 999,
        hb: 999,
        context_tokens: 0,
    };
    fs::write(&path, serde_json::to_string(&new).unwrap()).unwrap();
    assert!(
        !sem.reap_if_match(&path, Some(&old)),
        "a slot claimed/updated since evaluation must NOT be deleted"
    );
    assert!(path.exists(), "the new lease must survive the compare");
    assert_eq!(sem.read_lease(0).as_ref(), Some(&new));
}

/// If the slot file is **gone** by the time the re-read happens, the
/// compare is a no-op (nothing to delete, no spurious failure).
#[test]
fn reap_if_match_noop_when_file_gone() {
    let state = tmp_state();
    let sem = TaskSemaphore::new(&state, 2);
    let path = sem.slot_path(0);
    let evaluated = Lease {
        tenant: 4_999_999,
        task_id: "t".into(),
        at: 1,
        hb: 1,
        context_tokens: 0,
    };
    fs::write(&path, serde_json::to_string(&evaluated).unwrap()).unwrap();
    let _ = fs::remove_file(&path); // slot vanished between evaluation and re-read
    assert!(!sem.reap_if_match(&path, Some(&evaluated)));
}

/// `reap_slots` end-to-end: it must delete a stale slot (dead owner) whose
/// identity is stable, but must NOT touch a slot whose bound task is still
/// active under a live owner, and a second pass must be a no-op.
#[test]
fn reap_slots_compare_and_delete_end_to_end() {
    use crate::task::registry::{TaskRegistry, TaskStatus};
    let state = tmp_state();
    let reg = TaskRegistry::new(&state);
    let sem = TaskSemaphore::new(&state, 4);
    let me = std::process::id() as u32;

    // An active (non-terminal) task bound to a live owner → must be kept.
    let active = rt().block_on(reg.create("p", "/tmp", "s")).unwrap();
    let _ = rt()
        .block_on(reg.transition(&active, TaskStatus::Executing, None))
        .unwrap();
    fs::write(
        sem.slot_path(0),
        serde_json::to_string(&Lease {
            tenant: me,
            task_id: active,
            at: now_ms(),
            hb: now_ms(),
            context_tokens: 0,
        })
        .unwrap(),
    )
    .unwrap();

    // A dead-owner slot → must be deleted.
    let dead = Lease {
        tenant: 4_999_999,
        task_id: "t".into(),
        at: now_ms(),
        hb: now_ms(),
        context_tokens: 0,
    };
    fs::write(sem.slot_path(1), serde_json::to_string(&dead).unwrap()).unwrap();

    let reaped = sem.reap_slots();
    assert_eq!(reaped, 1, "only the dead-owner slot should be reclaimed");
    assert!(
        sem.slot_path(0).exists(),
        "the live-owner/active-task slot must be kept"
    );
    assert!(
        !sem.slot_path(1).exists(),
        "the dead-owner slot must be gone"
    );

    // Idempotent: a second pass reaps nothing.
    assert_eq!(sem.reap_slots(), 0);
}

/// With the pool budget disabled (`pool_capacity_tokens == 0`) the yield
/// gate never fires, regardless of the leases' contexts.
#[test]
fn should_yield_disabled_when_no_budget() {
    let state = tmp_state();
    let sem = TaskSemaphore::with_poll_interval(&state, 2, 50);
    let _a = rt().block_on(sem.acquire_with_tokens("a", 1_000));
    let b = rt().block_on(sem.acquire_with_tokens("b", 1_000)); // younger
    // No budget configured → the gate is disabled even though the
    // contexts are large.
    assert!(!sem.should_yield(&b));
}
