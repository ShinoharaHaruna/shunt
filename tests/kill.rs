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
