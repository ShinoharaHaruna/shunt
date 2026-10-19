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
    let q = Q::new("orphan");
    let m_orphan = q.root.join("orphan-ran");
    let m_normal = q.root.join("normal-ran");
    let order = q.root.join("order.txt");
    let mk = |tag: &str, marker: &Path| format!("#!/usr/bin/env bash\necho {tag} >> {}\ntouch {}\n", order.display(), marker.display());
    q.enqueue_script("old", &mk("old", &m_orphan));
    {
        let mut d = q.daemon();
        wait_until(|| !q.tasks("running").is_empty(), 10, "task running");
        // kill the daemon hard mid-flight: task left in running/
        let pid = q.daemon_pid().unwrap();
        let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
        let _ = d.wait();
        wait_until(|| !q.tasks("running").is_empty(), 5, "running stays");
    }
    // queue a normal task before restart
    q.enqueue_script("new", &mk("new", &m_normal));
    let mut d = q.daemon();
    wait_until(|| m_orphan.exists() && m_normal.exists(), 15, "both tasks ran");
    let lines: Vec<String> = std::fs::read_to_string(&order).unwrap().lines().map(|s| s.to_string()).collect();
    // at-least-once: the unguarded orphan may have already run once
    // before the daemon died, so "old" can appear twice; the point is
    // the requeued copy precedes the normally-numbered task.
    assert_eq!(lines.first().map(String::as_str), Some("old"), "{lines:?}");
    assert_eq!(lines.last().map(String::as_str), Some("new"), "{lines:?}");
    // interrupted log swept to failed/
    assert!(q.names("failed").iter().any(|n| n.contains("interrupted")), "expected interrupted log in failed/: {:?}", q.names("failed"));
    // double-requeue must not stack prefixes
    let requeued = q.names("done");
    assert!(!requeued.iter().any(|n| n.contains("requeued_0000_requeued")), "prefix stacked: {requeued:?}");
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
    // get reports resolved value + source
    let out = q.run_ok(&["workers"]);
    assert!(out.contains("workers:"), "got {out}");
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
