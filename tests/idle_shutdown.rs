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
fn idle_shutdown_spawn_failure_keeps_daemon_registered() {
    // design.md §3.11: when the shutdown command cannot even be
    // spawned, the daemon must stay visible — the old exec() path
    // removed daemon.pid and deregistered the qroot *before* exec,
    // so a launch failure left a headless daemon (invisible to
    // alive()/status/busy_peers). A PATH with no `sh` forces
    // Command::new("sh").spawn() to fail without any real poweroff.
    let q = Q::new("shutspawnfail");
    std::fs::write(q.root.join("shutdown_cmd"), "/nonexistent-xyz").unwrap();
    q.run_ok(&["shutdown", "on", "2"]);
    let empty_bin = q.root.join("empty-bin");
    std::fs::create_dir(&empty_bin).unwrap();
    let mut c = Command::new(common::bin());
    c.args(["daemon", "--root"]).arg(&q.root).env("POLL", "0.1").env("SHUNT_STATE", q.root.join("boxstate")).env("PATH", &empty_bin).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    let mut d = c.spawn().expect("spawn daemon");
    wait_until(|| q.dlog().contains("idle_shutdown_exec_failed"), 15, "spawn failure logged");
    assert!(q.daemon_pid().is_some(), "pidfile survives spawn failure");
    assert!(d.try_wait().unwrap().is_none(), "daemon still running after spawn failure");
    // A live daemon re-arms the countdown on the next poll cycle
    // rather than going headless.
    wait_until(|| q.dlog().matches("idle_shutdown_armed").count() >= 2, 10, "countdown re-armed");
    q.stop();
    let status = d.wait().unwrap();
    assert!(status.success());
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

#[test]
fn term_countdown_cancelled_by_pending_exits_loudly() {
    // design.md §3.5: pending was empty at signal time so the daemon
    // entered the honoring countdown, but a task arrived mid-countdown
    // and cancelled it — the flag is still armed with queued work, so
    // the signal exit must log signal_left_shutdown_armed instead of
    // falling through to exit(143) silently.
    let q = Q::new("termcancel");
    let fired = q.root.join("FIRED");
    std::fs::write(q.root.join("shutdown_cmd"), format!("touch {}", fired.display())).unwrap();
    // A running task keeps the pre-signal idle check from firing on
    // its own; workers=1 keeps the late task parked in pending/.
    std::fs::write(q.root.join("workers"), "1\n").unwrap();
    q.enqueue_script("long", &sleep_task());
    q.run_ok(&["shutdown", "on", "5"]);
    let mut d = q.daemon();
    wait_until(|| !q.tasks("running").is_empty(), 10, "task running");
    let pid = q.daemon_pid().unwrap();
    let _ = Command::new("kill").args(["-TERM", &pid.to_string()]).status();
    // The TERM'd task leaves running/ empty and pending/ is still
    // empty at the pre-check, so the honoring countdown starts —
    // then the late task cancels it.
    wait_until(|| q.dlog().contains("signal_honors_shutdown"), 10, "signal path enters honoring");
    q.enqueue_script("late", &sleep_task());
    wait_until(|| q.dlog().contains("idle_shutdown_cancelled"), 10, "countdown cancelled by pending");
    wait_until(|| q.dlog().contains("signal_left_shutdown_armed"), 10, "cancelled honoring must exit loudly");
    assert!(q.dlog().contains("\"warning\""), "{}", q.dlog());
    let status = d.wait().unwrap();
    assert_eq!(status.code(), Some(143), "signal exit code");
    assert!(q.root.join("shutdown_on_idle").exists(), "flag stays armed for a restarted daemon");
    assert!(!fired.exists(), "must not power off with pending work");
}

/// §4.1 `shared`: server-side arm refuses before any wire op —
/// idle poweroff on a shared box would kill other people's work.
#[test]
fn shutdown_on_refuses_shared_row() {
    let home = std::env::temp_dir().join(format!("shunt-shared-{}", std::process::id()));
    std::fs::create_dir_all(home.join("clients")).unwrap();
    std::fs::write(home.join("clients/sharedbox.toml"), "name = \"sharedbox\"\nlocal = true\nshared = true\nqroot = \"/tmp/q-sharedbox\"\ndeploy_root = \"/tmp\"\n").unwrap();
    let out = Command::new(common::bin()).args(["shutdown", "on", "-c", "sharedbox"]).env("SHUNT_HOME", &home).output().unwrap();
    assert!(!out.status.success(), "armed a shared row: {:?}", out.status);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("shared"), "{stderr}");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn exit_on_idle_leaves_immediately_when_drained() {
    // §3.11a: flag + already-empty queue → the daemon exits on its
    // first drain check instead of staying resident.
    let q = Q::new("exitidle");
    std::fs::write(q.root.join("exit_on_idle"), b"1\n").unwrap();
    let mut d = q.daemon();
    wait_until(|| d.try_wait().unwrap().is_some(), 15, "daemon exits on drain");
    assert!(d.wait().unwrap().success());
    assert!(q.dlog().contains("daemon_exit_idle"), "{}", q.dlog());
    assert!(q.daemon_pid().is_none(), "pidfile cleaned on exit");
}

#[test]
fn exit_on_idle_waits_for_running_task() {
    // §3.11a: the flag only drains the daemon — a dispatched task
    // keeps it resident until done/.
    let q = Q::new("exitidle-run");
    std::fs::write(q.root.join("exit_on_idle"), b"1\n").unwrap();
    q.enqueue_script("slow", "#!/usr/bin/env bash\nexec sleep 4\n");
    let mut d = q.daemon();
    wait_until(|| !q.tasks("running").is_empty(), 10, "task running");
    assert!(d.try_wait().unwrap().is_none(), "running task keeps daemon resident");
    wait_until(|| !q.tasks("done").is_empty(), 15, "task done");
    wait_until(|| d.try_wait().unwrap().is_some(), 15, "daemon exits after drain");
    assert!(q.dlog().contains("daemon_exit_idle"), "{}", q.dlog());
}

#[test]
fn idle_shutdown_blocks_unknown_registry_state() {
    for fault in ["directory", "missing", "unreadable-entry", "malformed-entry", "permissions"] {
        let qa = Q::new(&format!("unknown-{fault}-a"));
        let qb = Q::new(&format!("unknown-{fault}-b"));
        let state = qa.root.join("shared-state");
        let reg = state.join("qroots");
        let saved = state.join("saved-qroots");
        let release = qb.root.join("release");
        qb.enqueue_script("held", &format!("#!/bin/sh\nwhile [ ! -f {} ]; do sleep 0.1; done\n", release.display()));
        let mut db = qb.daemon_state(&state);
        wait_until(|| !qb.tasks("running").is_empty(), 10, "peer running");
        let mut da = qa.daemon_state(&state);
        wait_until(|| qa.dlog().contains("daemon_start"), 10, "idle daemon registered");
        let bad_entry = reg.join("bad-peer");
        match fault {
            "directory" | "missing" => {
                std::fs::rename(&reg, &saved).unwrap();
                if fault == "directory" {
                    std::fs::write(&reg, "not a directory").unwrap();
                }
            }
            "unreadable-entry" => std::fs::create_dir(&bad_entry).unwrap(),
            "malformed-entry" => std::fs::write(&bad_entry, "\n").unwrap(),
            "permissions" => {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&reg, std::fs::Permissions::from_mode(0o000)).unwrap();
                // Privileged test runners can still read mode 000; other faults
                // exercise the same gate without relying on Unix credentials.
                if std::fs::read_dir(&reg).is_ok() {
                    std::fs::set_permissions(&reg, std::fs::Permissions::from_mode(0o700)).unwrap();
                    std::fs::write(&release, b"").unwrap();
                    qa.stop();
                    qb.stop();
                    da.wait().unwrap();
                    db.wait().unwrap();
                    continue;
                }
            }
            _ => unreachable!(),
        }
        let fired = qa.root.join("FIRED");
        std::fs::write(qa.root.join("shutdown_cmd"), format!("touch {}", fired.display())).unwrap();
        qa.run_ok(&["shutdown", "on", "1"]);
        wait_until(|| fired.exists() || qa.dlog().contains("peer registry unavailable"), 10, "unknown registry blocks shutdown");
        let still_armed = qa.root.join("shutdown_on_idle").exists();
        // Disarm before any assertion so a failing test cannot fire on cleanup.
        qa.run_ok(&["shutdown", "off"]);
        let log = qa.dlog();
        let did_fire = fired.exists();
        match fault {
            "directory" | "missing" => {
                if fault == "directory" {
                    std::fs::remove_file(&reg).unwrap();
                }
                std::fs::rename(&saved, &reg).unwrap();
            }
            "unreadable-entry" => std::fs::remove_dir(&bad_entry).unwrap(),
            "malformed-entry" => std::fs::remove_file(&bad_entry).unwrap(),
            "permissions" => {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&reg, std::fs::Permissions::from_mode(0o700)).unwrap();
            }
            _ => unreachable!(),
        }
        let peer_still_running = !qb.tasks("running").is_empty();
        std::fs::write(&release, b"").unwrap();
        qa.stop();
        qb.stop();
        da.wait().unwrap();
        db.wait().unwrap();
        assert!(!did_fire, "{fault}: powered off with unknown peer state");
        assert!(still_armed, "registry errors must not disarm shutdown");
        assert!(log.contains(reg.to_str().unwrap()), "warning must identify the failing registry: {log}");
        assert!(log.contains("idle_shutdown_blocked") && log.contains("warning") && log.contains("repair"), "{fault}: {log}");
        assert!(peer_still_running, "peer task must stay held throughout the blocked shutdown");
    }
}

#[test]
fn daemon_refuses_to_run_without_registry_registration() {
    for fault in ["directory", "record"] {
        let q = Q::new(&format!("register-failed-{fault}"));
        let state = q.root.join("boxstate");
        std::fs::create_dir_all(&state).unwrap();
        if fault == "directory" {
            std::fs::write(state.join("qroots"), "not a directory").unwrap();
        } else {
            let mut first = q.daemon_state(&state);
            wait_until(|| q.dlog().contains("daemon_start"), 10, "initial registration");
            let entry = std::fs::read_dir(state.join("qroots")).unwrap().next().unwrap().unwrap().path();
            q.stop();
            first.wait().unwrap();
            std::fs::create_dir(entry).unwrap();
            std::fs::remove_file(q.root.join("daemon.log")).unwrap();
        }
        std::fs::write(q.root.join("shutdown_cmd"), "true").unwrap();
        let mut d = q.daemon_state(&state);
        wait_until(|| d.try_wait().unwrap().is_some() || q.dlog().contains("daemon_start"), 10, "startup outcome");
        let exited = d.try_wait().unwrap();
        if exited.is_none() {
            q.stop();
        }
        let status = d.wait().unwrap();
        assert!(exited.is_some() && !status.success(), "{fault}: daemon ran invisibly after registration failure");
        assert!(q.daemon_pid().is_none(), "failed startup must remove pidfile");
        let log = q.dlog();
        assert!(log.contains("qroot_registration_failed") && log.contains("repair"), "{log}");
    }
}
