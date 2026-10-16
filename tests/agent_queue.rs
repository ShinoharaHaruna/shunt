//! Client-queue integration tests (design.md §11): a real `shunt
//! daemon` against a tmp queue root, poll interval shortened via
//! `POLL`. Every wait is condition-based with a timeout; no fixed
//! sleeps are relied on.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_shunt")
}

static SEQ: AtomicUsize = AtomicUsize::new(0);

struct Q {
    root: PathBuf,
}

impl Q {
    fn new(tag: &str) -> Self {
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let root =
            std::env::temp_dir().join(format!("shunt-test-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let q = Q { root };
        q.run_ok(&["init"]);
        q
    }

    fn run(&self, args: &[&str]) -> Output {
        let mut c = Command::new(bin());
        let mut argv: Vec<String> = vec![
            args[0].to_string(),
            "--root".into(),
            self.root.to_string_lossy().into_owned(),
        ];
        argv.extend(args[1..].iter().map(|s| s.to_string()));
        c.args(&argv);
        c.output().expect("shunt run failed")
    }

    fn run_ok(&self, args: &[&str]) -> String {
        let o = self.run(args);
        assert!(
            o.status.success(),
            "shunt {args:?} failed: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8_lossy(&o.stdout).into_owned()
    }

    fn run_err(&self, args: &[&str]) -> String {
        let o = self.run(args);
        assert!(!o.status.success(), "shunt {args:?} unexpectedly ok");
        String::from_utf8_lossy(&o.stderr).into_owned()
    }

    /// Spawn a foreground `shunt daemon` we can signal directly.
    /// The returned Child is owned by the caller; Drop cleans up via
    /// the pidfile if a test panics before joining it.
    fn daemon(&self) -> Child {
        let mut c = Command::new(bin());
        c.args(["daemon", "--root"])
            .arg(&self.root)
            .env("POLL", "0.1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        c.spawn().expect("spawn daemon")
    }

    /// `shunt start` (self-daemonizing path).
    fn start(&self) -> String {
        let mut c = Command::new(bin());
        c.args(["start", "--root"])
            .arg(&self.root)
            .env("POLL", "0.1");
        let o = c.output().expect("shunt start failed");
        assert!(
            o.status.success(),
            "start failed: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8_lossy(&o.stdout).into_owned()
    }

    fn stop(&self) {
        let _ = self.run(&["stop"]);
    }

    fn enqueue_script(&self, slug: &str, body: &str) -> String {
        let f = self.root.join(format!("task-{slug}.sh.src"));
        std::fs::write(&f, body).unwrap();
        self.run_ok(&["enqueue", f.to_str().unwrap(), slug]);
        let mut names: Vec<String> = std::fs::read_dir(self.root.join("pending"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(slug))
            .collect();
        names.sort();
        names.pop().expect("task not enqueued")
    }

    fn names(&self, track: &str) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(self.root.join(track))
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v
    }

    /// Task files only (the `.sh` entries) in a track.
    fn tasks(&self, track: &str) -> Vec<String> {
        self.names(track)
            .into_iter()
            .filter(|n| n.ends_with(".sh"))
            .collect()
    }

    fn daemon_pid(&self) -> Option<u32> {
        std::fs::read_to_string(self.root.join("daemon.pid"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
    }

    fn dlog(&self) -> String {
        std::fs::read_to_string(self.root.join("daemon.log")).unwrap_or_default()
    }
}

impl Drop for Q {
    fn drop(&mut self) {
        // `shunt stop` drains gracefully; the pidfile kill is a
        // fallback for daemons detached via `shunt start` or stuck.
        self.stop();
        for _ in 0..20 {
            if self.daemon_pid().is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if let Some(pid) = self.daemon_pid() {
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
        }
    }
}

fn wait_until(mut cond: impl FnMut() -> bool, secs: u64, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for: {what}");
}

fn sleep_task() -> String {
    "#!/usr/bin/env bash\nexec sleep 30\n".to_string()
}

fn marker_task(marker: &Path) -> String {
    format!("#!/usr/bin/env bash\ntouch {}\n", marker.display())
}

#[test]
fn dispatch_order_priority_band_first() {
    let q = Q::new("order");
    std::fs::write(q.root.join("workers"), "1\n").unwrap();
    let order = q.root.join("order.txt");
    let body = |tag: &str| format!("#!/usr/bin/env bash\necho {tag} >> {}\n", order.display());
    // normal band entries
    q.enqueue_script("n1", &body("n1"));
    q.enqueue_script("n2", &body("n2"));
    // priority-band entry installed directly (0000-0099 runs first)
    std::fs::write(q.root.join("pending/0005_p0.sh"), body("p0")).unwrap();
    let mut d = q.daemon();
    wait_until(
        || {
            std::fs::read_to_string(&order)
                .map(|s| s.lines().count() == 3)
                .unwrap_or(false)
        },
        15,
        "all three tasks to run",
    );
    let lines: Vec<String> = std::fs::read_to_string(&order)
        .unwrap()
        .lines()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(lines, ["p0", "n1", "n2"]);
    q.stop();
    let _ = d.wait();
}

#[test]
fn enqueue_numbering_and_mode() {
    let q = Q::new("seq");
    for s in ["a", "b", "c"] {
        q.enqueue_script(s, &marker_task(&q.root.join(s)));
    }
    let names = q.names("pending");
    assert_eq!(names.len(), 3);
    assert!(names[0].starts_with("0100_"), "got {names:?}");
    assert!(names[2].starts_with("0102_"), "got {names:?}");
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(q.root.join("pending").join(&names[0]))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o755);
    // stdin wire form with --json ack
    let o = Command::new(bin())
        .args(["enqueue", "-", "wireslug", "--json", "--root"])
        .arg(&q.root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write;
            c.stdin
                .as_mut()
                .unwrap()
                .write_all(b"#!/usr/bin/env bash\nexit 0\n")?;
            c.wait_with_output()
        })
        .unwrap();
    assert!(o.status.success());
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).expect("ack is JSON");
    assert_eq!(v["protocol"], 1);
    assert_eq!(v["ok"], true);
    assert!(v["name"].as_str().unwrap().contains("wireslug"));
}

#[test]
fn cancel_zero_one_many() {
    let q = Q::new("cancel");
    q.enqueue_script("alpha", &sleep_task());
    q.enqueue_script("beta", &sleep_task());
    q.run_err(&["cancel", "nomatch"]);
    // "a" matches both alpha and beta
    q.run_err(&["cancel", "a_"]);
    let out = q.run_ok(&["cancel", "alpha"]);
    assert!(out.contains("cancelled:"), "got {out}");
    assert_eq!(q.names("pending").len(), 1);
}

#[test]
fn prio_renumber_and_refuse_existing() {
    let q = Q::new("prio");
    q.enqueue_script("one", &sleep_task());
    q.enqueue_script("two", &sleep_task());
    let out = q.run_ok(&["prio", "one", "5"]);
    assert!(out.contains("0005_one.sh"), "got {out}");
    // refuse to renumber onto an existing name
    std::fs::write(q.root.join("pending/0005_two.sh"), sleep_task()).unwrap();
    let err = q.run_err(&["prio", "0101", "5"]);
    assert!(err.contains("exists"), "got {err}");
    assert_eq!(q.tasks("pending").len(), 3, "task must not be clobbered");
}

#[test]
fn kill_term_lands_in_failed() {
    let q = Q::new("kill");
    q.enqueue_script("victim", &sleep_task());
    let mut d = q.daemon();
    wait_until(|| !q.tasks("running").is_empty(), 10, "task running");
    let out = q.run_ok(&["kill", "victim"]);
    assert!(out.contains("terminated"), "got {out}");
    wait_until(
        || q.names("failed").iter().any(|n| n.ends_with(".exit")),
        10,
        "task finalized to failed/",
    );
    let exit = q
        .names("failed")
        .into_iter()
        .find(|n| n.ends_with(".exit"))
        .unwrap();
    let rc: i32 = std::fs::read_to_string(q.root.join("failed").join(&exit))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(rc, 143, "TERM should record rc 143, got {rc}");
    q.stop();
    let _ = d.wait();
}

#[test]
fn kill_escalates_stubborn_task() {
    let q = Q::new("kill9");
    // ignores TERM entirely -> forces the KILL escalation path
    q.enqueue_script(
        "stubborn",
        "#!/usr/bin/env bash\ntrap '' TERM\nexec sleep 60\n",
    );
    let mut d = q.daemon();
    wait_until(|| !q.tasks("running").is_empty(), 10, "task running");
    let out = q.run_ok(&["kill", "stubborn"]);
    assert!(out.contains("SIGKILL"), "got {out}");
    wait_until(
        || q.names("failed").iter().any(|n| n.ends_with(".exit")),
        15,
        "killed task finalized",
    );
    q.stop();
    let _ = d.wait();
}

#[test]
fn stop_drains_then_exits() {
    let q = Q::new("stop");
    let marker = q.root.join("done-marker");
    // task finishes on its own after ~1 s
    q.enqueue_script(
        "short",
        &format!("#!/usr/bin/env bash\nsleep 1\ntouch {}\n", marker.display()),
    );
    let mut d = q.daemon();
    wait_until(|| !q.tasks("running").is_empty(), 10, "task running");
    q.stop();
    wait_until(|| q.daemon_pid().is_none(), 15, "daemon exits after drain");
    assert!(marker.exists(), "in-flight task must finish, not be killed");
    assert!(q.names("done").iter().any(|n| n.ends_with(".exit")));
    let status = d.wait().unwrap();
    assert!(status.success());
}

#[test]
fn orphan_requeue_front_of_queue() {
    let q = Q::new("orphan");
    let m_orphan = q.root.join("orphan-ran");
    let m_normal = q.root.join("normal-ran");
    let order = q.root.join("order.txt");
    let mk = |tag: &str, marker: &Path| {
        format!(
            "#!/usr/bin/env bash\necho {tag} >> {}\ntouch {}\n",
            order.display(),
            marker.display()
        )
    };
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
    wait_until(
        || m_orphan.exists() && m_normal.exists(),
        15,
        "both tasks ran",
    );
    let lines: Vec<String> = std::fs::read_to_string(&order)
        .unwrap()
        .lines()
        .map(|s| s.to_string())
        .collect();
    // at-least-once: the unguarded orphan may have already run once
    // before the daemon died, so "old" can appear twice; the point is
    // the requeued copy precedes the normally-numbered task.
    assert_eq!(lines.first().map(String::as_str), Some("old"), "{lines:?}");
    assert_eq!(lines.last().map(String::as_str), Some("new"), "{lines:?}");
    // interrupted log swept to failed/
    assert!(
        q.names("failed").iter().any(|n| n.contains("interrupted")),
        "expected interrupted log in failed/: {:?}",
        q.names("failed")
    );
    // double-requeue must not stack prefixes
    let requeued = q.names("done");
    assert!(
        !requeued
            .iter()
            .any(|n| n.contains("requeued_0000_requeued")),
        "prefix stacked: {requeued:?}"
    );
    q.stop();
    let _ = d.wait();
}

#[test]
fn orphan_live_pgid_warns_and_requeues() {
    let q = Q::new("livepgid");
    // craft a running/ entry by hand: dead-daemon leftover whose pgid
    // is still alive (a detached sleeper)
    use std::os::unix::process::CommandExt;
    let mut sleeper = Command::new("sleep")
        .arg("30")
        .process_group(0)
        .spawn()
        .unwrap();
    let pgid = sleeper.id();
    let running = q.root.join("running");
    std::fs::write(
        running.join("0001_ghost.sh"),
        marker_task(&q.root.join("ghost")),
    )
    .unwrap();
    std::fs::write(running.join("0001_ghost.sh.log"), b"partial\n").unwrap();
    std::fs::write(running.join("0001_ghost.sh.pid"), format!("{pgid}\n")).unwrap();

    let mut d = q.daemon();
    wait_until(
        || q.dlog().contains("orphan_live_pgid"),
        10,
        "WARNING about live pgid",
    );
    assert!(q.dlog().contains("\"warning\""), "{}", q.dlog());
    wait_until(
        || q.root.join("ghost").exists(),
        15,
        "requeued ghost task runs",
    );
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
    let mk = || -> String {
        format!(
            "#!/usr/bin/env bash\nwhile [ ! -e {} ]; do sleep 0.1; done\n",
            rel.display()
        )
    };
    q.enqueue_script("w1", &mk());
    q.enqueue_script("w2", &mk());
    let mut d = q.daemon();
    wait_until(|| q.tasks("running").len() == 1, 10, "one running");
    // raise -> second task dispatches immediately
    q.run_ok(&["workers", "2"]);
    wait_until(
        || q.tasks("running").len() == 2,
        10,
        "two running after raise",
    );
    // lower while both run -> drain only, no kill
    q.run_ok(&["workers", "1"]);
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(
        q.tasks("running").len(),
        2,
        "lower must not kill running tasks"
    );
    // invalid value falls back to last-known (still runs, no crash)
    std::fs::write(q.root.join("workers"), "bogus\n").unwrap();
    std::fs::write(&rel, b"").unwrap();
    wait_until(
        || {
            q.names("done")
                .iter()
                .filter(|n| n.ends_with(".exit"))
                .count()
                == 2
        },
        15,
        "both tasks drain",
    );
    // get reports resolved value + source
    let out = q.run_ok(&["workers"]);
    assert!(out.contains("workers:"), "got {out}");
    q.stop();
    let _ = d.wait();
}

#[test]
fn env_overlay_workdir_threads_proxy() {
    let q = Q::new("env");
    let workdir = q.root.join("wd");
    std::fs::create_dir_all(&workdir).unwrap();
    let out = q.root.join("env-out.txt");
    std::fs::write(
        q.root.join("env"),
        format!(
            "WORKDIR={}\nTASK_THREADS=2\nFOO=bar\nPATH_PREPEND=/nonexistent-prepend\n",
            workdir.display()
        ),
    )
    .unwrap();
    q.enqueue_script(
        "envcheck",
        &format!(
            "#!/usr/bin/env bash\n{{ echo \"pwd=$PWD\"; echo \"omp=$OMP_NUM_THREADS\"; echo \"foo=$FOO\"; echo \"proxies=$(env | grep -cE '^(ALL_PROXY|all_proxy|http_proxy|https_proxy|HTTP_PROXY|HTTPS_PROXY)=')\"; echo \"path=$PATH\"; }} > {}\n",
            out.display()
        ),
    );
    let mut c = Command::new(bin());
    c.args(["daemon", "--root"])
        .arg(&q.root)
        .env("POLL", "0.1")
        .env("http_proxy", "http://should-be-unset:1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut d = c.spawn().unwrap();
    wait_until(|| out.exists(), 15, "env task ran");
    let text = std::fs::read_to_string(&out).unwrap();
    assert!(
        text.contains(&format!("pwd={}", workdir.display())),
        "{text}"
    );
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
    wait_until(
        || q.names("failed").iter().any(|n| n.ends_with(".exit")),
        15,
        "task finalized",
    );
    let exit = q
        .names("failed")
        .into_iter()
        .find(|n| n.ends_with(".exit"))
        .unwrap();
    let rc = std::fs::read_to_string(q.root.join("failed").join(&exit)).unwrap();
    assert_eq!(rc.trim(), "97");
    q.stop();
    let _ = d.wait();
}

#[test]
fn idle_shutdown_fires_marker() {
    let q = Q::new("shutfire");
    let fired = q.root.join("FIRED");
    std::fs::write(
        q.root.join("shutdown_cmd"),
        format!("touch {}", fired.display()),
    )
    .unwrap();
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
    std::fs::write(
        q.root.join("shutdown_cmd"),
        format!("touch {}", fired.display()),
    )
    .unwrap();
    q.run_ok(&["shutdown", "on", "4"]);
    let mut d = q.daemon();
    wait_until(|| q.dlog().contains("idle_shutdown_armed"), 10, "armed");
    q.run_ok(&["shutdown", "off"]);
    wait_until(
        || q.dlog().contains("idle_shutdown_cancelled"),
        10,
        "cancelled",
    );
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
    std::fs::write(
        q.root.join("shutdown_cmd"),
        format!("touch {}", fired.display()),
    )
    .unwrap();
    q.run_ok(&["shutdown", "on", "4"]);
    let mut d = q.daemon();
    wait_until(|| q.dlog().contains("idle_shutdown_armed"), 10, "armed");
    q.enqueue_script("late", &marker_task(&ran));
    wait_until(
        || q.dlog().contains("idle_shutdown_cancelled"),
        10,
        "cancelled by pending",
    );
    wait_until(|| ran.exists(), 15, "late task dispatched after cancel");
    assert!(!fired.exists());
    q.stop();
    let _ = d.wait();
}

#[test]
fn idle_shutdown_cancel_stop() {
    let q = Q::new("shutstop");
    let fired = q.root.join("FIRED");
    std::fs::write(
        q.root.join("shutdown_cmd"),
        format!("touch {}", fired.display()),
    )
    .unwrap();
    q.run_ok(&["shutdown", "on", "4"]);
    let mut d = q.daemon();
    wait_until(|| q.dlog().contains("idle_shutdown_armed"), 10, "armed");
    q.stop();
    wait_until(|| q.daemon_pid().is_none(), 10, "daemon exits via STOP");
    assert!(!fired.exists());
    let status = d.wait().unwrap();
    assert!(status.success());
}

#[test]
fn status_json_shape() {
    let q = Q::new("status");
    q.enqueue_script("s1", &sleep_task());
    let o = Command::new(bin())
        .args(["status", "--json", "--root"])
        .arg(&q.root)
        .output()
        .unwrap();
    assert!(o.status.success());
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["protocol"], 1);
    assert_eq!(v["daemon"]["alive"], false);
    assert_eq!(v["workers"]["value"], 3);
    assert_eq!(v["workers"]["source"], "file");
    assert_eq!(v["pending"].as_array().unwrap().len(), 1);
    assert_eq!(v["stop"], false);
    assert_eq!(v["shutdown"]["armed"], false);
}

#[test]
fn mktask_and_new() {
    let q = Q::new("mktask");
    let out = q.run_ok(&[
        "mktask",
        "--slug",
        "gen",
        "--guard",
        "runs/x.json",
        "--",
        "echo",
        "a b",
    ]);
    assert!(out.contains("set -uo pipefail"), "{out}");
    assert!(out.contains("[ -e runs/x.json ]"), "{out}");
    assert!(out.contains("exec echo 'a b'"), "{out}");
    assert!(!out.contains("\ncd "), "no cd line allowed: {out}");
    q.run_ok(&["new", "--slug", "gen", "--", "echo", "hi"]);
    assert_eq!(q.names("pending").len(), 1);
    let name = &q.names("pending")[0];
    let body = std::fs::read_to_string(q.root.join("pending").join(name)).unwrap();
    assert!(body.contains("exec echo hi"));
}

#[test]
fn task_threads_auto_uses_cgroup() {
    let q = Q::new("threadsauto");
    let out = q.root.join("threads.txt");
    std::fs::write(q.root.join("env"), "TASK_THREADS=auto\n").unwrap();
    q.enqueue_script(
        "tcap",
        &format!(
            "#!/usr/bin/env bash\necho \"$OMP_NUM_THREADS\" > {}\n",
            out.display()
        ),
    );
    let mut d = q.daemon();
    wait_until(|| out.exists(), 15, "task ran");
    let n: u32 = std::fs::read_to_string(&out)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(n >= 1);
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
fn enqueue_concurrent_no_duplicate_seq() {
    let q = Q::new("conc");
    let body = marker_task(&q.root.join("m"));
    let f = q.root.join("task-conc.sh.src");
    std::fs::write(&f, &body).unwrap();
    let mut handles = Vec::new();
    for i in 0..8 {
        let root = q.root.clone();
        let f = f.clone();
        handles.push(std::thread::spawn(move || {
            Command::new(bin())
                .args(["enqueue", f.to_str().unwrap(), &format!("c{i}"), "--root"])
                .arg(&root)
                .output()
                .unwrap()
        }));
    }
    for h in handles {
        let o = h.join().unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    }
    let names = q.tasks("pending");
    assert_eq!(names.len(), 8);
    let mut seqs: Vec<String> = names
        .iter()
        .map(|n| n.split('_').next().unwrap().to_string())
        .collect();
    seqs.sort();
    seqs.dedup();
    assert_eq!(seqs.len(), 8, "duplicate seq numbers: {names:?}");
}
