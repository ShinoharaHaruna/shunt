//! Idle-shutdown arming/cancel/firing and the honoring paths
//! (STOP drain, signal, multi-qroot peer coordination). Every
//! shutdown goes through the `shutdown_cmd` marker redirect —
//! nothing here may reach the real /usr/bin/shutdown.

mod common;

use common::{marker_task, sleep_task, wait_until, Q, SEQ};
use std::process::Command;
use std::sync::atomic::Ordering;

#[test]
fn idle_shutdown_fires_marker() {
    let q = Q::new("shutfire");
    let fired = q.root.join("FIRED");
    std::fs::write(q.root.join("shutdown_cmd"), format!("touch {}", fired.display())).unwrap();
    q.run_ok(&["shutdown", "on", "2"]);
    let mut d = q.daemon();
    wait_until(|| q.dlog().contains("idle_shutdown_armed"), 10, "armed");
    wait_until(|| fired.exists(), 15, "shutdown command fired");
    // exec replaced the daemon
    wait_until(|| q.daemon_pid().is_none(), 5, "daemon exec'd away");
    assert!(q.dlog().contains("idle_shutdown_exec"));
    let _ = d.wait();
}

#[test]
fn idle_shutdown_cancel_flag_removed() {
    let q = Q::new("shutflag");
    let fired = q.root.join("FIRED");
    std::fs::write(q.root.join("shutdown_cmd"), format!("touch {}", fired.display())).unwrap();
    q.run_ok(&["shutdown", "on", "4"]);
    let mut d = q.daemon();
    wait_until(|| q.dlog().contains("idle_shutdown_armed"), 10, "armed");
    q.run_ok(&["shutdown", "off"]);
    wait_until(|| q.dlog().contains("idle_shutdown_cancelled"), 10, "cancelled");
    assert!(!fired.exists());
    assert!(q.daemon_pid().is_some(), "daemon still alive after cancel");
    q.stop();
    let _ = d.wait();
}

#[test]
fn idle_shutdown_cancel_new_pending() {
    let q = Q::new("shutpend");
    let fired = q.root.join("FIRED");
    let ran = q.root.join("ran");
    std::fs::write(q.root.join("shutdown_cmd"), format!("touch {}", fired.display())).unwrap();
    q.run_ok(&["shutdown", "on", "4"]);
    let mut d = q.daemon();
    wait_until(|| q.dlog().contains("idle_shutdown_armed"), 10, "armed");
    q.enqueue_script("late", &marker_task(&ran));
    wait_until(|| q.dlog().contains("idle_shutdown_cancelled"), 10, "cancelled by pending");
    wait_until(|| ran.exists(), 15, "late task dispatched after cancel");
    assert!(!fired.exists());
    q.stop();
    let _ = d.wait();
}

#[test]
fn stop_drain_honors_armed_shutdown() {
    // design.md §3.10: STOP drain must not orphan an armed
    // shutdown_on_idle flag — the daemon runs the grace countdown
    // and fires before exiting.
    let q = Q::new("stopshut");
    let fired = q.root.join("FIRED");
    std::fs::write(q.root.join("shutdown_cmd"), format!("touch {}", fired.display())).unwrap();
    q.run_ok(&["shutdown", "on", "2"]);
    // STOP must be in place before the daemon starts: if the daemon's
    // first poll cycle sees the armed flag on an empty queue before
    // STOP lands, it enters the *normal* countdown and STOP then
    // cancels + disarms (§3.11) — the flag is gone by drain-exit and
    // nothing is honored. STOP-first selects the §3.10 honoring path
    // deterministically.
    q.stop();
    let mut d = q.daemon();
    wait_until(|| q.dlog().contains("stop_drain_honors_shutdown"), 10, "drain honors armed flag");
    wait_until(|| fired.exists(), 15, "shutdown command fired after drain");
    wait_until(|| q.daemon_pid().is_none(), 5, "daemon exec'd away");
    assert!(q.dlog().contains("idle_shutdown_exec"));
    let _ = d.wait();
}

#[test]
fn stop_drain_honored_shutdown_still_cancellable() {
    // During the honoring countdown the flag can still be disarmed;
    // the daemon then exits 0 without firing.
    let q = Q::new("stopshutoff");
    let fired = q.root.join("FIRED");
    std::fs::write(q.root.join("shutdown_cmd"), format!("touch {}", fired.display())).unwrap();
    q.run_ok(&["shutdown", "on", "6"]);
    // STOP placed before the daemon starts (same race as
    // stop_drain_honors_armed_shutdown) so the countdown the test
    // observes is the honoring countdown, not a normal one.
    q.stop();
    let mut d = q.daemon();
    wait_until(|| q.dlog().contains("idle_shutdown_armed"), 10, "honoring countdown started");
    q.run_ok(&["shutdown", "off"]);
    wait_until(|| q.dlog().contains("idle_shutdown_cancelled"), 10, "cancelled");
    wait_until(|| q.daemon_pid().is_none(), 10, "daemon exits after cancel");
    assert!(!fired.exists());
    let status = d.wait().unwrap();
    assert!(status.success());
}

#[test]
fn idle_shutdown_blocked_by_busy_sibling_qroot() {
    // design.md §3.11: two qroots sharing one box coordinate via the
    // box-level registry — a drained daemon must not power off while
    // a sibling daemon still has work in flight.
    let state = std::env::temp_dir().join(format!("shunt-test-boxstate-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::SeqCst)));
    let qa = Q::new("peerA");
    let qb = Q::new("peerB");
    // Sibling daemon with a task held on a release marker.
    let release = qb.root.join("release");
    let done_b = qb.root.join("b-done");
    qb.enqueue_script("held", &format!("#!/usr/bin/env bash\nwhile [ ! -f {} ]; do sleep 0.2; done\ntouch {}\n", release.display(), done_b.display()));
    let mut db = qb.daemon_state(&state);
    wait_until(|| !qb.tasks("running").is_empty(), 10, "sibling task running");

    let fired = qa.root.join("FIRED");
    std::fs::write(qa.root.join("shutdown_cmd"), format!("touch {}", fired.display())).unwrap();
    qa.run_ok(&["shutdown", "on", "2"]);
    let mut da = qa.daemon_state(&state);
    wait_until(|| qa.dlog().contains("idle_shutdown_blocked"), 15, "blocked by busy peer");
    assert!(!fired.exists(), "must not power off while a sibling runs");

    // The sibling finishes; a live-but-idle daemon does NOT block —
    // a forgotten idle daemon must not pin the box up forever.
    std::fs::write(&release, b"").unwrap();
    wait_until(|| done_b.exists(), 15, "sibling task finished");
    wait_until(|| fired.exists(), 15, "fires once the box drains");
    assert!(qa.dlog().contains("idle_shutdown_exec"));
    let _ = da.wait();
    qb.stop();
    let _ = db.wait();
    let _ = std::fs::remove_dir_all(&state);
}

#[test]
fn idle_shutdown_cancel_stop() {
    let q = Q::new("shutstop");
    let fired = q.root.join("FIRED");
    std::fs::write(q.root.join("shutdown_cmd"), format!("touch {}", fired.display())).unwrap();
    q.run_ok(&["shutdown", "on", "4"]);
    let mut d = q.daemon();
    wait_until(|| q.dlog().contains("idle_shutdown_armed"), 10, "armed");
    q.stop();
    wait_until(|| q.daemon_pid().is_none(), 10, "daemon exits via STOP");
    assert!(!fired.exists());
    assert!(!q.root.join("shutdown_on_idle").exists(), "STOP-cancel disarms the flag");
    let status = d.wait().unwrap();
    assert!(status.success());
}

#[test]
fn term_honors_armed_shutdown() {
    // design.md §3.5: a signal-killed daemon must not orphan an armed
    // shutdown_on_idle flag — after forwarding TERM→KILL it honors the
    // flag (grace countdown, then fire) instead of exiting with the
    // box left billing idle.
    let q = Q::new("termshut");
    let fired = q.root.join("FIRED");
    std::fs::write(q.root.join("shutdown_cmd"), format!("touch {}", fired.display())).unwrap();
    // A running task keeps the idle check from firing on its own, so
    // the honoring must come from the signal path.
    q.enqueue_script("long", &sleep_task());
    q.run_ok(&["shutdown", "on", "2"]);
    let mut d = q.daemon();
    wait_until(|| !q.tasks("running").is_empty(), 10, "task running");
    let pid = q.daemon_pid().unwrap();
    let _ = Command::new("kill").args(["-TERM", &pid.to_string()]).status();
    wait_until(|| q.dlog().contains("signal_honors_shutdown"), 10, "signal path honors armed flag");
    // the TERM'd task is finalized to failed/, not left as an orphan
    wait_until(|| !q.exits("failed").is_empty(), 10, "killed task finalized");
    let (_, rc) = q.exits("failed").into_iter().next().unwrap();
    assert_eq!(rc, 143, "forwarded TERM should record rc 143, got {rc}");
    wait_until(|| fired.exists(), 15, "shutdown command fired after grace");
    wait_until(|| q.daemon_pid().is_none(), 5, "daemon gone after exec");
    assert!(q.dlog().contains("idle_shutdown_exec"));
    let _ = d.wait();
}

#[test]
fn term_with_pending_leaves_flag_armed_loudly() {
    // design.md §3.5: with pending work the daemon must not power off
    // (queued tasks would be stranded), so the flag stays armed and a
    // WARNING event records the hand-off — silent orphaning was the bug.
    let q = Q::new("termpend");
    let fired = q.root.join("FIRED");
    std::fs::write(q.root.join("shutdown_cmd"), format!("touch {}", fired.display())).unwrap();
    std::fs::write(q.root.join("workers"), "1\n").unwrap();
    q.enqueue_script("run", &sleep_task());
    q.enqueue_script("queued", &sleep_task());
    q.run_ok(&["shutdown", "on", "2"]);
    let mut d = q.daemon();
    wait_until(|| !q.tasks("running").is_empty(), 10, "first task running");
    let pid = q.daemon_pid().unwrap();
    let _ = Command::new("kill").args(["-TERM", &pid.to_string()]).status();
    wait_until(|| q.dlog().contains("signal_left_shutdown_armed"), 10, "WARNING event logged");
    assert!(q.dlog().contains("\"warning\""), "{}", q.dlog());
    let status = d.wait().unwrap();
    assert_eq!(status.code(), Some(143), "signal exit code");
    assert!(q.root.join("shutdown_on_idle").exists(), "flag stays armed for a restarted daemon");
    assert!(!fired.exists(), "must not power off with pending work");
    assert_eq!(q.tasks("pending").len(), 1, "queued task survives for the next daemon");
}
