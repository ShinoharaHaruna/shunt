use crate::client::exec;
use fs2::FileExt;

/// Fail closed on an unreadable pending/ (§3.11): `pending` as a
/// plain file makes pending_list error (ENOTDIR); dispatch must
/// skip the cycle without spawning, and the event must be in
/// daemon.log — a silent "empty" would feed idle-shutdown a
/// phantom drained queue.
#[test]
fn dispatch_skips_cycle_on_unreadable_pending() {
    let root = std::env::temp_dir().join(format!("shunt-dispatch-test-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("pending"), b"not a dir").unwrap();
    let mut d = super::core::Daemon { root: root.clone(), runpids: Default::default(), last_workers: None, got_signal: Default::default(), reexec: Default::default(), argv_rest: Default::default() };
    d.dispatch();
    assert!(d.runpids.is_empty(), "nothing may dispatch off an unreadable pending/");
    let log = std::fs::read_to_string(root.join("daemon.log")).unwrap();
    assert!(log.contains("pending_scan_failed"), "scan failure must be loud in daemon.log");
    let _ = std::fs::remove_dir_all(&root);
}

/// A rescheduled entry must not be claimed until `retry_after`
/// passes (§3.9): a future timestamp skips it this cycle, an
/// expired or corrupt one claims it and clears the file.
#[test]
fn dispatch_skips_not_due_rescheduled_entry() {
    let root = std::env::temp_dir().join(format!("shunt-retrydue-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
    crate::client::queue::init(&root).unwrap();
    let entry = root.join("pending/0100_t");
    std::fs::create_dir_all(&entry).unwrap();
    std::fs::write(entry.join("task.sh"), "#!/usr/bin/env bash\nexec true\n").unwrap();
    let mut d = super::core::Daemon { root: root.clone(), runpids: Default::default(), last_workers: None, got_signal: Default::default(), reexec: Default::default(), argv_rest: Default::default() };
    // Not yet due: entry stays pending, nothing spawns.
    std::fs::write(entry.join("retry_after"), format!("{}\n", crate::proto::time::unix_now() + 3600)).unwrap();
    d.dispatch();
    assert!(d.runpids.is_empty() && entry.is_dir(), "not-due entry must wait in pending/");
    // Due now: claimed and the marker cleared.
    std::fs::write(entry.join("retry_after"), format!("{}\n", crate::proto::time::unix_now() - 1)).unwrap();
    d.dispatch();
    assert_eq!(d.runpids.len(), 1, "due entry must claim");
    assert!(!root.join("running/0100_t/retry_after").exists(), "retry_after must be cleared on claim");
    for (pgid, _) in d.runpids.drain() {
        // No Child is held in the handle (leaders reap via waitpid):
        // kill the group and wait by pgid.
        unsafe {
            libc::kill(-(pgid as i32), libc::SIGKILL);
            let mut st = 0;
            libc::waitpid(pgid as i32, &mut st, 0);
        }
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// A held entry remains pending until release; claim rechecks the
/// gate under the same lock and never carries the marker into running/.
#[test]
fn dispatch_skips_held_entry_until_release_then_claims_it() {
    let root = std::env::temp_dir().join(format!("shunt-held-dispatch-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
    crate::client::queue::init(&root).unwrap();
    let entry = root.join("pending/0100_t");
    std::fs::create_dir_all(&entry).unwrap();
    std::fs::write(entry.join("task.sh"), "#!/usr/bin/env bash\nexec sleep 60\n").unwrap();
    std::fs::write(entry.join("held"), "await approval").unwrap();
    let mut d = super::core::Daemon { root: root.clone(), runpids: Default::default(), last_workers: None, got_signal: Default::default(), reexec: Default::default(), argv_rest: Default::default() };

    d.dispatch();
    assert!(d.runpids.is_empty());
    assert_eq!(crate::client::queue::pending_list(&root).unwrap(), vec!["0100_t"]);
    assert!(crate::client::shutdown::pending_nonempty(&root), "held work must still block idle shutdown");

    crate::client::queue::release_pending(&root, "0100_t").unwrap();
    d.dispatch();
    assert_eq!(d.runpids.len(), 1);
    assert!(!root.join("running/0100_t/held").exists(), "claim must consume the admission marker");
    for (pgid, _) in d.runpids.drain() {
        unsafe {
            libc::kill(-(pgid as i32), libc::SIGKILL);
            let mut st = 0;
            libc::waitpid(pgid as i32, &mut st, 0);
        }
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// §3.7/§3.9 timeout marker honesty: when the deadline has passed
/// but no TERM can be delivered (here: no pid file — the
/// pid_write_failed blind spot), `timed_out`/`kill_at` must stay
/// unset and a `timeout_unenforceable` WARNING logged. The marker
/// means "killed for exceeding budget"; stamping it on a task
/// that was never signaled would force a clean rc-0 finish into
/// failed/ as a fake budget death.
#[test]
fn unenforceable_timeout_stays_unmarked() {
    let root = std::env::temp_dir().join(format!("shunt-timeout-test-{}", std::process::id()));
    std::fs::create_dir_all(root.join("running")).unwrap();
    let mut child = std::process::Command::new("sleep").arg("60").spawn().unwrap();
    let pid = child.id();
    let h = super::core::RunHandle { base: "t-timeout".into(), dirs: exec::TaskDirectives { timeout_s: Some(1), ..Default::default() }, deadline: Some(std::time::Instant::now() - std::time::Duration::from_secs(1)), usage: Default::default(), last_jiffies: 0, timed_out: None, kill_at: None, gone: false, unenforceable_warned: 0, exited: None };
    let mut d = super::core::Daemon { root: root.clone(), runpids: [(pid, h)].into_iter().collect(), last_workers: None, got_signal: Default::default(), reexec: Default::default(), argv_rest: Default::default() };
    d.enforce_timeouts();
    let h = &d.runpids[&pid];
    assert!(h.timed_out.is_none() && h.kill_at.is_none(), "no signal delivered — nothing may be marked");
    // Second poll while still unenforceable: the WARNING is a
    // once-per-state event, not one line per cycle.
    d.enforce_timeouts();
    let log = std::fs::read_to_string(root.join("daemon.log")).unwrap();
    let n = log.matches("timeout_unenforceable").count();
    assert_eq!(n, 1, "undeliverable TERM must be loud once, not once per poll ({n} lines)");
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&root);
}

/// §3.13 remnant escalation: a task already marked timed_out whose
/// leader died on TERM but whose detached grandchild holds the pgid
/// must still get the kill_at KILL — the §3.9a gate accepts a
/// leader-dead-group-live remnant because a live pgid cannot be
/// recycled. Before the fix, `exited` skipped the whole enforce
/// pass and the escalation never ran.
#[test]
fn exited_timeout_still_escalates_kill_to_remnant_group() {
    use std::os::unix::process::CommandExt;
    let root = std::env::temp_dir().join(format!("shunt-remnant-kill-test-{}", std::process::id()));
    let entry = root.join("running/t-remnant");
    std::fs::create_dir_all(&entry).unwrap();
    // Leader spawns a TERM-immune sleeper in its own process group
    // (process_group(0) -> pgid = leader pid, children inherit) and
    // exits — the `cmd &` remnant pattern from tests/kill.rs.
    let mut child = std::process::Command::new("bash").args(["-c", "(trap '' TERM; exec sleep 300) & exit 0"]).process_group(0).spawn().unwrap();
    let pgid = child.id();
    let _ = child.wait();
    // Wait for the remnant to be running so the group is live at
    // enforce time; the leader is already dead.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !exec::pgid_alive(pgid) {
        assert!(std::time::Instant::now() < deadline, "remnant group never came up");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    // Pid file with no start_time: the leader is dead so only the
    // remnant clause of the §3.9a gate can pass.
    std::fs::write(entry.join("pid"), format!("{pgid}\n")).unwrap();
    let now = std::time::Instant::now();
    let h = super::core::RunHandle { base: "t-remnant".into(), dirs: exec::TaskDirectives { timeout_s: Some(1), ..Default::default() }, deadline: Some(now - std::time::Duration::from_secs(20)), usage: Default::default(), last_jiffies: 0, timed_out: Some(1), kill_at: Some(now - std::time::Duration::from_secs(1)), gone: false, unenforceable_warned: 0, exited: Some(super::core::Exited { rc: 143, deadline: now + std::time::Duration::from_secs(60) }) };
    let mut d = super::core::Daemon { root: root.clone(), runpids: [(pgid, h)].into_iter().collect(), last_workers: None, got_signal: Default::default(), reexec: Default::default(), argv_rest: Default::default() };
    d.enforce_timeouts();
    let h = &d.runpids[&pgid];
    assert!(h.kill_at.is_none(), "elapsed kill_at must be consumed");
    let log = std::fs::read_to_string(root.join("daemon.log")).unwrap();
    assert!(log.contains("task_timeout_kill"), "remnant group must get the KILL escalation: {log}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while exec::pgid_alive(pgid) {
        assert!(std::time::Instant::now() < deadline, "remnant group survived its timeout KILL");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// §3.5 loud-orphan predicate behind the signal-exit re-check:
/// only flag-armed AND pending-nonempty logs the WARNING — a
/// disarmed flag or an empty pending/ is not an orphan.
#[test]
fn warn_orphaned_flag_only_when_armed_with_pending() {
    let root = std::env::temp_dir().join(format!("shunt-orphan-test-{}", std::process::id()));
    std::fs::create_dir_all(root.join("pending")).unwrap();
    // No flag: nothing armed, nothing to warn about.
    super::exit::warn_orphaned_shutdown_flag(&root);
    assert!(!root.join("daemon.log").exists(), "no flag, no log");
    // Flag armed but pending/ empty: honored or disarmed, not orphaned.
    std::fs::write(root.join("shutdown_on_idle"), b"").unwrap();
    super::exit::warn_orphaned_shutdown_flag(&root);
    assert!(!root.join("daemon.log").exists(), "empty pending must not warn");
    // Flag armed + pending work: the orphan must be loud.
    std::fs::create_dir_all(root.join("pending/t1")).unwrap();
    super::exit::warn_orphaned_shutdown_flag(&root);
    let log = std::fs::read_to_string(root.join("daemon.log")).unwrap();
    assert!(log.contains("signal_left_shutdown_armed"), "{log}");
    assert!(log.contains("\"warning\""), "{log}");
    let _ = std::fs::remove_dir_all(&root);
}

/// Startup mutex (§3.4): a held daemon.lock means a live daemon —
/// run() must bail "already running" immediately rather than
/// proceeding to the pidfile and spawning a twin. flock releases
/// on process death, so the failed start leaves nothing behind.
#[test]
fn held_daemon_lock_bails_already_running() {
    let root = std::env::temp_dir().join(format!("shunt-dlock-test-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let lock = std::fs::OpenOptions::new().create(true).append(true).open(root.join("daemon.lock")).unwrap();
    lock.try_lock_exclusive().unwrap();
    let err = super::core::run(&root).unwrap_err();
    assert!(format!("{err:#}").contains("daemon already running"), "unexpected error: {err:#}");
    drop(lock);
    let _ = std::fs::remove_dir_all(&root);
}
