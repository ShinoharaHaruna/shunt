//! Daemon lifecycle: start/stop/drain, orphan requeue, workers
//! resize, claim/finalize failure quarantine paths.

mod common;

use common::{marker_task, sleep_task, wait_until, Q};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

#[test]
fn stop_drains_then_exits() {
    let q = Q::new("stop");
    let marker = q.root.join("done-marker");
    // task finishes on its own after ~1 s
    q.enqueue_script("short", &format!("#!/usr/bin/env bash\nsleep 1\ntouch {}\n", marker.display()));
    let mut d = q.daemon();
    wait_until(|| !q.tasks("running").is_empty(), 10, "task running");
    q.stop();
    wait_until(|| q.daemon_pid().is_none(), 15, "daemon exits after drain");
    assert!(marker.exists(), "in-flight task must finish, not be killed");
    assert!(!q.exits("done").is_empty());
    let status = d.wait().unwrap();
    assert!(status.success());
}

#[test]
fn orphan_requeue_front_of_queue() {
    // A leftover running/ entry is crafted by hand instead of racing a
    // live task against `kill -9`: two flakes lived there — the task
    // could finalize before the -9 landed (no orphan, no interrupted
    // log), and with the default workers=3 the requeued orphan and the
    // normal task are claimed in the same dispatch cycle, so their
    // order-file writes interleave nondeterministically. workers=1
    // serializes the check this test actually makes: requeue sorts
    // ahead of the normal sequence.
    let q = Q::new("orphan");
    std::fs::write(q.root.join("workers"), "1\n").unwrap();
    let m_orphan = q.root.join("orphan-ran");
    let m_normal = q.root.join("normal-ran");
    let order = q.root.join("order.txt");
    let mk = |tag: &str, marker: &Path| format!("#!/usr/bin/env bash\necho {tag} >> {}\ntouch {}\n", order.display(), marker.display());
    // dead-daemon leftover: dir entry with task.sh + an in-flight log
    std::fs::create_dir_all(q.root.join("running/0001_old")).unwrap();
    std::fs::write(q.root.join("running/0001_old/task.sh"), mk("old", &m_orphan)).unwrap();
    std::fs::write(q.root.join("running/0001_old/log"), b"partial\n").unwrap();
    std::fs::write(q.root.join("running/0001_old/pid"), b"99999999\n").unwrap();
    q.enqueue_script("new", &mk("new", &m_normal));
    let mut d = q.daemon();
    wait_until(|| m_orphan.exists() && m_normal.exists(), 15, "both tasks ran");
    let lines: Vec<String> = std::fs::read_to_string(&order).unwrap().lines().map(|s| s.to_string()).collect();
    // the requeued copy lands at 0000_requeued_* and runs first
    assert_eq!(lines, ["old", "new"], "{lines:?}");
    // interrupted log swept to failed/ as <clean>.interrupted.<ts>.log
    // (§3.6) — the timestamp must survive, not be eaten as the
    // "extension" (regression: with_extension("log") clobbered it and
    // successive interruptions overwrote each other).
    let interrupted: Vec<_> = q.names("failed").into_iter().filter(|n| n.contains("interrupted")).collect();
    assert!(interrupted.iter().any(|n| n.strip_prefix("0001_old.interrupted.").is_some_and(|r| r.strip_suffix(".log").is_some_and(|ts| ts.chars().all(|c| c.is_ascii_digit()) && !ts.is_empty()))), "expected <clean>.interrupted.<unixts>.log in failed/: {interrupted:?}");
    // double-requeue must not stack prefixes
    let requeued = q.names("done");
    assert!(!requeued.iter().any(|n| n.contains("requeued_0000_requeued")), "prefix stacked: {requeued:?}");
    q.stop();
    let _ = d.wait();
}

#[test]
fn leader_exit_lingering_pgid_finalize_deferred() {
    // The leader exits while a grandchild keeps the process group
    // alive: finalize must wait out the group (bounded) so the
    // sidecars stay addressable and kill can still reach it.
    let q = Q::new("linger");
    q.enqueue_script("linger", "#!/usr/bin/env bash\nsleep 1 &\nexit 0\n");
    let mut d = q.daemon();
    wait_until(|| !q.tasks("running").is_empty(), 10, "task running");
    std::thread::sleep(Duration::from_millis(300));
    assert!(q.tasks("running").iter().any(|n| n.contains("linger")), "entry must not finalize while the pgid is still alive: {:?}", q.tasks("running"));
    wait_until(|| q.exits("done").iter().any(|(n, _)| n.contains("linger")), 15, "finalized once the group drained");
    assert!(!q.dlog().contains("finalize_lingering_pgid"), "grace must not fire for a drained group");
    q.stop();
    let _ = d.wait();
}

#[test]
fn lingering_pgid_grace_expiry_warns_and_finalizes() {
    // Grandchild outlives the drain grace: finalize anyway, but leave
    // a WARNING naming the residual pgid for audit.
    let q = Q::new("linger2");
    q.enqueue_script("linger", "#!/usr/bin/env bash\nsleep 30 &\nexit 0\n");
    let mut c = Command::new(common::bin());
    let mut d = c.args(["daemon", "--root"]).arg(&q.root).env("POLL", "0.1").env("SHUNT_PGID_DRAIN_S", "1").env("SHUNT_STATE", q.root.join("boxstate")).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().expect("spawn daemon");
    wait_until(|| q.dlog().contains("finalize_lingering_pgid"), 15, "lingering-pgid WARNING logged");
    assert!(q.exits("done").iter().any(|(n, _)| n.contains("linger")), "entry finalized despite live grandchild");
    q.stop();
    let _ = d.wait();
}

#[test]
fn orphan_live_pgid_warns_and_requeues() {
    let q = Q::new("livepgid");
    // craft a running/ entry by hand: dead-daemon leftover whose pgid
    // is still alive (a detached sleeper)
    use std::os::unix::process::CommandExt;
    let mut sleeper = Command::new("sleep").arg("30").process_group(0).spawn().unwrap();
    let pgid = sleeper.id();
    let running = q.root.join("running");
    std::fs::write(running.join("0001_ghost.sh"), marker_task(&q.root.join("ghost"))).unwrap();
    std::fs::write(running.join("0001_ghost.sh.log"), b"partial\n").unwrap();
    std::fs::write(running.join("0001_ghost.sh.pid"), format!("{pgid}\n")).unwrap();

    let mut d = q.daemon();
    wait_until(|| q.dlog().contains("orphan_live_pgid"), 10, "WARNING about live pgid");
    assert!(q.dlog().contains("\"warning\""), "{}", q.dlog());
    wait_until(|| q.root.join("ghost").exists(), 15, "requeued ghost task runs");
    q.stop();
    let _ = d.wait();
    let _ = sleeper.kill();
    let _ = sleeper.wait();
}

#[test]
fn workers_resize_raise_lower_invalid() {
    let q = Q::new("workers");
    std::fs::write(q.root.join("workers"), "1\n").unwrap();
    let rel = q.root.join("release"); // tasks wait on this marker
    let mk = || -> String { format!("#!/usr/bin/env bash\nwhile [ ! -e {} ]; do sleep 0.1; done\n", rel.display()) };
    q.enqueue_script("w1", &mk());
    q.enqueue_script("w2", &mk());
    let mut d = q.daemon();
    wait_until(|| q.tasks("running").len() == 1, 10, "one running");
    // raise -> second task dispatches immediately
    q.run_ok(&["workers", "2"]);
    wait_until(|| q.tasks("running").len() == 2, 10, "two running after raise");
    // lower while both run -> drain only, no kill
    q.run_ok(&["workers", "1"]);
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(q.tasks("running").len(), 2, "lower must not kill running tasks");
    // invalid value falls back to last-known (still runs, no crash)
    std::fs::write(q.root.join("workers"), "bogus\n").unwrap();
    std::fs::write(&rel, b"").unwrap();
    wait_until(|| q.exits("done").len() == 2, 15, "both tasks drain");
    // get reports resolved value + source — garbage file falls back
    // to the daemon's persisted last-known (design.md §3.8), so
    // status and the daemon agree instead of showing "default".
    let out = q.run_ok(&["workers"]);
    assert!(out.contains("workers: 1 (last-known)"), "got {out}");
    q.stop();
    let _ = d.wait();
}

#[test]
fn start_daemonizes_idempotent() {
    let q = Q::new("start");
    let m1 = q.root.join("ran1");
    q.enqueue_script("s", &marker_task(&m1));
    q.start();
    wait_until(|| q.daemon_pid().is_some(), 10, "pidfile written");
    let pid1 = q.daemon_pid().unwrap();
    // second start is a no-op returning the live pid
    q.start();
    assert_eq!(q.daemon_pid(), Some(pid1));
    wait_until(|| m1.exists(), 15, "task ran under daemonized daemon");
    q.stop();
    wait_until(|| q.daemon_pid().is_none(), 10, "daemon exited");
}

#[test]
fn unclaimable_pending_entry_quarantined_not_spun() {
    // A pending entry that cannot be renamed into running/ (name
    // collision with a leftover non-empty running dir) must be
    // quarantined and skipped — spinning on it would wedge the whole
    // dispatch loop and starve the tasks behind it.
    let q = Q::new("claimfail");
    let ran = q.root.join("next-ran");
    let mut d = q.daemon();
    // Wait for the startup sweep to finish, then create the
    // collision — requeue_orphans only runs at daemon start.
    wait_until(|| q.dlog().contains("workers_now"), 10, "daemon startup done");
    std::fs::create_dir_all(q.root.join("running/0050_stuck")).unwrap();
    std::fs::write(q.root.join("running/0050_stuck/task.sh"), sleep_task()).unwrap();
    std::fs::create_dir_all(q.root.join("pending/0050_stuck")).unwrap();
    std::fs::write(q.root.join("pending/0050_stuck/task.sh"), sleep_task()).unwrap();
    q.enqueue_script("next", &marker_task(&ran));
    wait_until(|| ran.exists(), 15, "task behind the stuck head still dispatched");
    wait_until(|| q.dlog().contains("claim_failed"), 10, "claim_failed event logged");
    assert!(q.tasks("pending").is_empty(), "stuck entry must leave pending/: {:?}", q.tasks("pending"));
    let quarantined = q.root.join("failed/0050_stuck");
    assert!(quarantined.is_dir(), "stuck entry quarantined to failed/");
    assert!(quarantined.join("shunt_error").exists(), "quarantine note written");
    assert!(q.daemon_pid().is_some(), "daemon still alive");
    q.stop();
    let _ = d.wait();
}

#[test]
fn failed_finalize_quarantines_instead_of_ghosting() {
    // If the finalize rename fails (here: done/ is unwritable), the
    // entry must not be left pid-less in running/ where kill and
    // status can't see it — it is moved to failed/ with a note.
    let q = Q::new("finfail");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(q.root.join("done"), std::fs::Permissions::from_mode(0o555)).unwrap();
    let marker = q.root.join("task-ran");
    q.enqueue_script("victim", &marker_task(&marker));
    let mut d = q.daemon();
    wait_until(|| q.dlog().contains("finalize_failed"), 15, "finalize_failed event logged");
    wait_until(|| q.tasks("running").is_empty(), 10, "no ghost left in running/");
    let failed = q.names("failed");
    let entry = failed.iter().find(|n| n.contains("victim")).unwrap_or_else(|| panic!("quarantined entry in failed/: {failed:?}"));
    assert!(q.root.join("failed").join(entry).join("shunt_error").exists(), "quarantine note written: {entry}");
    q.stop();
    let _ = d.wait();
    let _ = std::fs::set_permissions(q.root.join("done"), std::fs::Permissions::from_mode(0o755));
}

#[test]
fn spawn_failure_finalize_failure_quarantines() {
    // A task whose shell cannot be exec'd fails spawn and is
    // finalized inline; if that finalize rename also fails (here: a
    // non-empty failed/<name> collision), the entry must not be left
    // pid-less in running/ — same ghost rule as the reap path
    // (design.md §3.9).
    let q = Q::new("spawnfinfail");
    let name = q.enqueue_script("victim", &marker_task(&q.root.join("never-ran")));
    std::fs::write(q.root.join("shell"), "/nonexistent/shunt-test-shell").unwrap();
    std::fs::create_dir_all(q.root.join("failed").join(&name)).unwrap();
    std::fs::write(q.root.join("failed").join(&name).join("blocker"), "x").unwrap();
    let mut d = q.daemon();
    wait_until(|| q.dlog().contains("spawn_failed"), 15, "spawn_failed event logged");
    wait_until(|| q.dlog().contains("finalize_failed"), 10, "finalize_failed event logged");
    wait_until(|| q.tasks("running").is_empty(), 10, "no ghost left in running/");
    let failed = q.names("failed");
    let entry = failed.iter().find(|n| n.contains("victim") && n.contains("quarantined")).unwrap_or_else(|| panic!("quarantined entry in failed/: {failed:?}"));
    assert!(q.root.join("failed").join(entry).join("shunt_error").exists(), "quarantine note written: {entry}");
    q.stop();
    let _ = d.wait();
}

#[test]
fn orphan_requeue_collision_quarantines() {
    // A running/ leftover whose requeue target in pending/ is already
    // taken must land in failed/ with a note, not stay an invisible
    // ghost (design.md §3.6 rename-failure path).
    let q = Q::new("orphcoll");
    std::fs::create_dir_all(q.root.join("running/0200_orph")).unwrap();
    std::fs::write(q.root.join("running/0200_orph/task.sh"), sleep_task()).unwrap();
    // Conflicting non-empty dir at the requeue target name — the
    // requeue target is 0000_requeued_<basename> = 0000_requeued_0200_orph.
    std::fs::create_dir_all(q.root.join("pending/0000_requeued_0200_orph")).unwrap();
    std::fs::write(q.root.join("pending/0000_requeued_0200_orph/task.sh"), sleep_task()).unwrap();
    let mut d = q.daemon();
    wait_until(|| q.dlog().contains("requeue_failed"), 10, "requeue_failed event logged");
    assert!(q.tasks("running").is_empty() || !q.tasks("running").iter().any(|n| n == "0200_orph"), "orphan quarantined out of running/: {:?}", q.tasks("running"));
    assert!(q.root.join("failed/0200_orph").is_dir(), "orphan landed in failed/");
    assert!(q.root.join("failed/0200_orph/shunt_error").exists());
    q.stop();
    let _ = d.wait();
}
