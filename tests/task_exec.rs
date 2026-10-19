//! Task execution environment: env overlay, WORKDIR, TASK_THREADS,
//! env_setup sourcing, and timeout enforcement.

mod common;

use common::{bin, sleep_task, wait_until, Q};
use std::process::{Command, Stdio};

#[test]
fn env_overlay_workdir_threads_proxy() {
    let q = Q::new("env");
    let workdir = q.root.join("wd");
    std::fs::create_dir_all(&workdir).unwrap();
    let out = q.root.join("env-out.txt");
    std::fs::write(q.root.join("env"), format!("WORKDIR={}\nTASK_THREADS=2\nFOO=bar\nPATH_PREPEND=/nonexistent-prepend\n", workdir.display())).unwrap();
    q.enqueue_script("envcheck", &format!("#!/usr/bin/env bash\n{{ echo \"pwd=$PWD\"; echo \"omp=$OMP_NUM_THREADS\"; echo \"foo=$FOO\"; echo \"proxies=$(env | grep -cE '^(ALL_PROXY|all_proxy|http_proxy|https_proxy|HTTP_PROXY|HTTPS_PROXY)=')\"; echo \"path=$PATH\"; }} > {}\n", out.display()));
    let mut c = Command::new(bin());
    c.args(["daemon", "--root"]).arg(&q.root).env("POLL", "0.1").env("SHUNT_STATE", q.root.join("boxstate")).env("http_proxy", "http://should-be-unset:1").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let mut d = c.spawn().unwrap();
    wait_until(|| out.exists(), 15, "env task ran");
    let text = std::fs::read_to_string(&out).unwrap();
    assert!(text.contains(&format!("pwd={}", workdir.display())), "{text}");
    assert!(text.contains("omp=2"), "{text}");
    assert!(text.contains("foo=bar"), "{text}");
    assert!(text.contains("proxies=0"), "{text}");
    assert!(text.contains("path=/nonexistent-prepend:"), "{text}");
    q.stop();
    let _ = d.wait();
}

#[test]
fn workdir_unreachable_rc97() {
    let q = Q::new("rc97");
    std::fs::write(q.root.join("env"), "WORKDIR=/definitely/not/a/dir\n").unwrap();
    q.enqueue_script("nowd", &sleep_task());
    let mut d = q.daemon();
    wait_until(|| !q.exits("failed").is_empty(), 15, "task finalized");
    let (_, rc) = q.exits("failed").into_iter().next().unwrap();
    assert_eq!(rc, 97);
    q.stop();
    let _ = d.wait();
}

#[test]
fn task_threads_auto_uses_cgroup() {
    let q = Q::new("threadsauto");
    let out = q.root.join("threads.txt");
    std::fs::write(q.root.join("env"), "TASK_THREADS=auto\n").unwrap();
    q.enqueue_script("tcap", &format!("#!/usr/bin/env bash\necho \"$OMP_NUM_THREADS\" > {}\n", out.display()));
    let mut d = q.daemon();
    wait_until(|| out.exists(), 15, "task ran");
    let n: u32 = std::fs::read_to_string(&out).unwrap().trim().parse().unwrap();
    assert!(n >= 1);
    q.stop();
    let _ = d.wait();
}

#[test]
fn env_setup_sourced_before_task() {
    // `<qroot>/env_setup` is sourced in the task wrapper before exec
    // (design.md §3.12): a var exported there lands in the task env.
    let q = Q::new("envsetup");
    std::fs::write(q.root.join("env_setup"), "export SHUNT_SETUP_RAN=yes-i-am-sourced\n").unwrap();
    let marker = q.root.join("envsetup.txt");
    q.enqueue_script("probe", &format!("#!/usr/bin/env bash\necho \"$SHUNT_SETUP_RAN\" > {}\n", marker.display()));
    let mut d = q.daemon();
    wait_until(|| !q.exits("done").is_empty(), 15, "task done");
    q.stop();
    let _ = d.wait();
    assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), "yes-i-am-sourced");
}

#[test]
fn env_setup_failure_never_blocks_task() {
    // A snippet whose last command fails returns nonzero but does not
    // block the task (the wrapper isn't `set -e`).
    let q = Q::new("envsetup-bad");
    std::fs::write(q.root.join("env_setup"), "echo setup-partial >&2\nfalse\n").unwrap();
    let marker = q.root.join("envsetup-ok.txt");
    q.enqueue_script("probe", &format!("#!/usr/bin/env bash\ntouch {}\n", marker.display()));
    let mut d = q.daemon();
    wait_until(|| !q.exits("done").is_empty(), 15, "task done");
    q.stop();
    let _ = d.wait();
    assert!(marker.exists());
}

#[test]
fn timeout_kills_runaway_task() {
    // design.md §3.13: a task past its `# shunt:timeout_s=` budget is
    // TERM'd, escalated to KILL after grace, and lands in failed/
    // with a `timeout` marker — distinguishable from a plain non-zero
    // exit.
    let q = Q::new("timeout");
    q.enqueue_script("runaway", "#!/usr/bin/env bash\n# shunt:timeout_s=1\nsleep 300\n");
    let mut d = q.daemon();
    wait_until(|| !q.exits("failed").is_empty(), 15, "timed-out task finalized to failed/");
    let (name, rc) = q.exits("failed").into_iter().next().unwrap();
    assert!(rc == 143 || rc == 137, "expected signal rc 143/137, got {rc}");
    let marker = q.root.join("failed").join(&name).join("timeout");
    assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), "1");
    let log = std::fs::read_to_string(q.root.join("failed").join(&name).join("log")).unwrap();
    assert!(log.contains("=== timeout: exceeded wall-clock limit 1s"), "{log}");
    assert!(q.dlog().contains("task_timeout"), "{}", q.dlog());
    q.stop();
    let _ = d.wait();
}

#[test]
fn guard_hit_task_exits_before_timeout() {
    // The guard fires in seconds, well inside the budget: timeout
    // must not turn a clean skip into a failure.
    let q = Q::new("guardto");
    let guard = q.root.join("already.done");
    std::fs::write(&guard, "").unwrap();
    q.enqueue_script("guarded", &format!("#!/usr/bin/env bash\n# shunt:timeout_s=1\nif [ -e {} ]; then echo \"guard hit\"; exit 0; fi\nsleep 300\n", guard.display()));
    let mut d = q.daemon();
    wait_until(|| !q.exits("done").is_empty(), 15, "guard-hit task finalized to done/");
    assert!(q.exits("failed").is_empty(), "guard-hit task must not fail");
    assert!(!q.dlog().contains("task_timeout"), "{}", q.dlog());
    q.stop();
    let _ = d.wait();
}

#[test]
fn exit_just_before_deadline_is_not_timed_out() {
    // Regression: the poll loop reaps before enforcing deadlines —
    // a leader that exited cleanly but whose try_wait has not run
    // yet must not be stamped timed_out and sent to failed/.
    let q = Q::new("edge");
    q.enqueue_script("edge", "#!/usr/bin/env bash\n# shunt:timeout_s=1\nsleep 0.9\necho ok\n");
    let mut d = q.daemon();
    wait_until(|| !q.exits("done").is_empty() || !q.exits("failed").is_empty(), 15, "task finalized");
    assert!(q.exits("failed").is_empty(), "clean exit at the deadline boundary must not fail: {:?}", q.exits("failed"));
    assert!(!q.dlog().contains("task_timeout"), "{}", q.dlog());
    q.stop();
    let _ = d.wait();
}

#[test]
fn no_timeout_task_still_runs() {
    // Regression: a task with no directive is never touched by the
    // supervisor.
    let q = Q::new("notimeout");
    q.enqueue_script("plain", "#!/usr/bin/env bash\nsleep 2\necho ok\n");
    let mut d = q.daemon();
    wait_until(|| !q.exits("done").is_empty(), 15, "plain task done");
    assert!(!q.dlog().contains("task_timeout"), "{}", q.dlog());
    q.stop();
    let _ = d.wait();
}
