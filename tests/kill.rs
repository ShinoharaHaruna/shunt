//! `shunt kill`: TERM/KILL escalation, pgid-recycling TOCTOU guard,
//! dead-leader "gone" reporting.

mod common;

use common::{bin, sleep_task, wait_until, Q};
use std::process::Command;

#[test]
fn kill_term_lands_in_failed() {
    let q = Q::new("kill");
    q.enqueue_script("victim", &sleep_task());
    let mut d = q.daemon();
    wait_until(|| !q.tasks("running").is_empty(), 10, "task running");
    let out = q.run_ok(&["kill", "victim"]);
    assert!(out.contains("terminated"), "got {out}");
    wait_until(|| !q.exits("failed").is_empty(), 10, "task finalized to failed/");
    let (_, rc) = q.exits("failed").into_iter().next().unwrap();
    assert_eq!(rc, 143, "TERM should record rc 143, got {rc}");
    q.stop();
    let _ = d.wait();
}

#[test]
fn kill_escalates_stubborn_task() {
    let q = Q::new("kill9");
    // ignores TERM entirely -> forces the KILL escalation path
    q.enqueue_script("stubborn", "#!/usr/bin/env bash\ntrap '' TERM\nexec sleep 60\n");
    let mut d = q.daemon();
    wait_until(|| !q.tasks("running").is_empty(), 10, "task running");
    let out = q.run_ok(&["kill", "stubborn"]);
    assert!(out.contains("SIGKILL"), "got {out}");
    wait_until(|| !q.exits("failed").is_empty(), 15, "killed task finalized");
    q.stop();
    let _ = d.wait();
}

#[test]
fn kill_refuses_recycled_pgid() {
    // TOCTOU guard: a pid file whose recorded leader start_time no
    // longer matches the live process means the pgid was recycled —
    // signaling it would hit a stranger's process group. kill must
    // report "gone" and send nothing.
    let q = Q::new("killgone");
    use std::os::unix::process::CommandExt;
    let mut stranger = Command::new("sleep").arg("30").process_group(0).spawn().unwrap();
    let foreign_pgid = stranger.id();
    std::fs::create_dir_all(q.root.join("running/0300_victim")).unwrap();
    std::fs::write(q.root.join("running/0300_victim/task.sh"), sleep_task()).unwrap();
    // bogus start_time: live process exists at this pid but it isn't
    // the task leader we recorded.
    std::fs::write(q.root.join("running/0300_victim/pid"), format!("{foreign_pgid} 1\n")).unwrap();
    let out = q.run_ok(&["kill", "victim"]);
    assert!(out.contains("gone"), "got {out}");
    let _ = Command::new("kill").args(["-0", &foreign_pgid.to_string()]).status().map(|s| {
        assert!(s.success(), "foreign process group was signaled");
    });
    // wire form is honest too
    let o = Command::new(bin()).args(["kill", "--json", "--root"]).arg(&q.root).args(["--", "victim"]).output().unwrap();
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["gone"], true, "{v}");
    let _ = stranger.kill();
    let _ = stranger.wait();
}

#[test]
fn kill_dead_leader_still_kills_remnant_group() {
    // §3.9a remnant case: the leader exits while a detached
    // grandchild keeps the process group alive (the `cmd &`
    // pattern). The pgid still provably names the task's group — a
    // live pgid cannot be recycled — so kill must TERM, then
    // escalate to KILL instead of reporting the remnant unreachable.
    let q = Q::new("killremnant");
    use std::os::unix::process::CommandExt;
    // Leader spawns a TERM-immune sleeper in its own process group
    // (process_group(0) → pgid = leader pid, children inherit) and
    // exits immediately — mimicking a task leader that dies on TERM.
    let leader = Command::new("bash").args(["-c", "(trap '' TERM; exec sleep 300) & exit 0"]).process_group(0).spawn().unwrap();
    let pgid = leader.id();
    let mut leader = leader;
    let _ = leader.wait();
    // Wait for the remnant member to be running before writing the
    // pid file, so the group is demonstrably live at kill time.
    wait_until(|| Command::new("kill").args(["-0", "--", &format!("-{pgid}")]).status().map(|s| s.success()).unwrap_or(false), 5, "remnant group alive");
    std::fs::create_dir_all(q.root.join("running/0302_remnant")).unwrap();
    std::fs::write(q.root.join("running/0302_remnant/task.sh"), sleep_task()).unwrap();
    std::fs::write(q.root.join("running/0302_remnant/pid"), format!("{pgid}\n")).unwrap();
    let out = q.run_ok(&["kill", "remnant"]);
    assert!(out.contains("SIGKILL"), "leader-dead remnant must escalate to KILL, got {out}");
    // The remnant group must actually be dead, not just reported.
    wait_until(|| !Command::new("kill").args(["-0", "--", &format!("-{pgid}")]).status().map(|s| s.success()).unwrap_or(false), 5, "remnant group reaped");
}

#[test]
fn kill_dead_leader_reports_gone() {
    // The task's leader pid is simply dead (already reaped/finalized
    // elsewhere): kill must not claim a kill.
    let q = Q::new("killdead");
    // a pid that is (practically) never live: spawn+reap a child
    let dead = Command::new("true").spawn().unwrap().wait_with_output().unwrap();
    let _ = dead;
    let dead_pid = {
        let c = Command::new("true").spawn().unwrap();
        let id = c.id();
        let mut c = c;
        let _ = c.wait();
        id
    };
    std::fs::create_dir_all(q.root.join("running/0301_dead")).unwrap();
    std::fs::write(q.root.join("running/0301_dead/task.sh"), sleep_task()).unwrap();
    std::fs::write(q.root.join("running/0301_dead/pid"), format!("{dead_pid}\n")).unwrap();
    let out = q.run_ok(&["kill", "dead"]);
    assert!(out.contains("gone"), "got {out}");
}
