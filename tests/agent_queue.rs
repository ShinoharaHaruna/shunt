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
        let root = std::env::temp_dir().join(format!("shunt-test-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let q = Q { root };
        q.run_ok(&["init"]);
        q
    }

    fn run(&self, args: &[&str]) -> Output {
        let mut c = Command::new(bin());
        let mut argv: Vec<String> = vec![args[0].to_string(), "--root".into(), self.root.to_string_lossy().into_owned()];
        argv.extend(args[1..].iter().map(|s| s.to_string()));
        c.args(&argv).env("SHUNT_STATE", self.root.join("boxstate"));
        c.output().expect("shunt run failed")
    }

    fn run_ok(&self, args: &[&str]) -> String {
        let o = self.run(args);
        assert!(o.status.success(), "shunt {args:?} failed: {}", String::from_utf8_lossy(&o.stderr));
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
    ///
    /// `SHUNT_STATE` isolates the box-level qroot registry per test:
    /// daemons from parallel tests must not see each other as peers.
    fn daemon(&self) -> Child {
        self.daemon_state(&self.root.join("boxstate"))
    }

    /// Same as `daemon()` but with an explicit shared `SHUNT_STATE` —
    /// two queues on one "box" for peer-coordination tests.
    fn daemon_state(&self, state: &Path) -> Child {
        let mut c = Command::new(bin());
        c.args(["daemon", "--root"]).arg(&self.root).env("POLL", "0.1").env("SHUNT_STATE", state).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        c.spawn().expect("spawn daemon")
    }

    /// `shunt start` (self-daemonizing path).
    fn start(&self) -> String {
        let mut c = Command::new(bin());
        c.args(["start", "--root"]).arg(&self.root).env("POLL", "0.1").env("SHUNT_STATE", self.root.join("boxstate"));
        let o = c.output().expect("shunt start failed");
        assert!(o.status.success(), "start failed: {}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).into_owned()
    }

    fn stop(&self) {
        let _ = self.run(&["stop"]);
    }

    fn enqueue_script(&self, slug: &str, body: &str) -> String {
        let f = self.root.join(format!("task-{slug}.sh.src"));
        std::fs::write(&f, body).unwrap();
        self.run_ok(&["enqueue", f.to_str().unwrap(), slug]);
        let mut names: Vec<String> = std::fs::read_dir(self.root.join("pending")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.contains(slug)).collect();
        names.sort();
        names.pop().expect("task not enqueued")
    }

    fn names(&self, track: &str) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(self.root.join(track)).map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
        v.sort();
        v
    }

    /// Task entries in a track: dir entries plus legacy `.sh` stems.
    fn tasks(&self, track: &str) -> Vec<String> {
        self.names(track).into_iter().filter_map(|n| if self.root.join(track).join(&n).is_dir() { Some(n) } else { n.strip_suffix(".sh").map(|s| s.to_string()) }).collect()
    }

    /// Entries carrying an `exit` file: `(name, rc)`.
    fn exits(&self, track: &str) -> Vec<(String, i32)> {
        self.tasks(track)
            .into_iter()
            .filter_map(|n| {
                let ef = if self.root.join(track).join(&n).is_dir() { self.root.join(track).join(&n).join("exit") } else { self.root.join(track).join(format!("{n}.sh.exit")) };
                std::fs::read_to_string(&ef).ok()?.trim().parse::<i32>().ok().map(|rc| (n, rc))
            })
            .collect()
    }

    fn daemon_pid(&self) -> Option<u32> {
        std::fs::read_to_string(self.root.join("daemon.pid")).ok().and_then(|s| s.trim().parse().ok())
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
    wait_until(|| std::fs::read_to_string(&order).map(|s| s.lines().count() == 3).unwrap_or(false), 15, "all three tasks to run");
    let lines: Vec<String> = std::fs::read_to_string(&order).unwrap().lines().map(|s| s.to_string()).collect();
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
    let mode = std::fs::metadata(q.root.join("pending").join(&names[0]).join("task.sh")).unwrap().permissions().mode();
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
            c.stdin.as_mut().unwrap().write_all(b"#!/usr/bin/env bash\nexit 0\n")?;
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
    assert!(out.contains("0005_one"), "got {out}");
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
fn status_json_shape() {
    let q = Q::new("status");
    q.enqueue_script("s1", &sleep_task());
    let o = Command::new(bin()).args(["status", "--json", "--root"]).arg(&q.root).output().unwrap();
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
    let out = q.run_ok(&["mktask", "--slug", "gen", "--guard", "runs/x.json", "--", "echo", "a b"]);
    assert!(out.contains("set -uo pipefail"), "{out}");
    assert!(out.contains("[ -e runs/x.json ]"), "{out}");
    assert!(out.contains("exec echo 'a b'"), "{out}");
    assert!(!out.contains("\ncd "), "no cd line allowed: {out}");
    q.run_ok(&["new", "--slug", "gen", "--", "echo", "hi"]);
    assert_eq!(q.names("pending").len(), 1);
    let name = &q.names("pending")[0];
    let body = std::fs::read_to_string(q.root.join("pending").join(name).join("task.sh")).unwrap();
    assert!(body.contains("exec echo hi"));
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
        handles.push(std::thread::spawn(move || Command::new(bin()).args(["enqueue", f.to_str().unwrap(), &format!("c{i}"), "--root"]).arg(&root).output().unwrap()));
    }
    for h in handles {
        let o = h.join().unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    }
    let names = q.tasks("pending");
    assert_eq!(names.len(), 8);
    let mut seqs: Vec<String> = names.iter().map(|n| n.split('_').next().unwrap().to_string()).collect();
    seqs.sort();
    seqs.dedup();
    assert_eq!(seqs.len(), 8, "duplicate seq numbers: {names:?}");
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
fn seq_wrap_stays_four_digits_and_fifo() {
    // design.md §3.2: the 4-digit namespace tops out at 9999. After
    // the counter passes it, allocation continues just past the
    // highest pending number so claim order stays FIFO — a 5-digit
    // `10000_x` would sort ahead of `9999_*` and leapfrog the queue.
    let q = Q::new("seqwrap");
    std::fs::write(q.root.join("pending/0500_held.sh"), sleep_task()).unwrap();
    std::fs::write(q.root.join(".seq"), "9999\n").unwrap();
    let name = q.enqueue_script("n1", &sleep_task());
    assert_eq!(name, "0501_n1", "wrapped seq must continue past the pending max, got {name}");
    // pending reaching 9999 leaves no name that sorts last: refuse.
    std::fs::write(q.root.join("pending/9999_top.sh"), sleep_task()).unwrap();
    let f = q.root.join("task-n2.sh.src");
    std::fs::write(&f, sleep_task()).unwrap();
    let err = q.run_err(&["enqueue", f.to_str().unwrap(), "n2"]);
    assert!(err.contains("sequence"), "got {err}");
    assert_eq!(q.tasks("pending").len(), 3, "failed enqueue must not install a task");
    assert!(q.names("pending").iter().all(|n| n.split('_').next().unwrap().len() == 4), "5-digit name leaked: {:?}", q.names("pending"));
}

#[test]
fn prio_rejects_outside_band() {
    // design.md §3.2: prio moves a task INTO 0000-0099; renumbering
    // into the normal sequence would collide with .seq allocation.
    let q = Q::new("prioband");
    q.enqueue_script("one", &sleep_task());
    let err = q.run_err(&["prio", "one", "123"]);
    assert!(err.contains("priority band"), "got {err}");
    let err = q.run_err(&["prio", "one", "100"]);
    assert!(err.contains("priority band"), "got {err}");
    // band edges still work
    let out = q.run_ok(&["prio", "one", "0"]);
    assert!(out.contains("0000_one"), "got {out}");
    let out = q.run_ok(&["prio", "one", "99"]);
    assert!(out.contains("0099_one"), "got {out}");
}

#[test]
fn slug_leading_dash_rejected() {
    // A `-`-leading slug parses as a flag in the remote argv; entries
    // rejected there stay in spool retrying forever (design.md §3.3).
    let q = Q::new("dashslug");
    let f = q.root.join("task.sh.src");
    std::fs::write(&f, sleep_task()).unwrap();
    // Wire form (`--` before the positionals, as dispatch sends it):
    // the check must produce a clean nonzero exit, not a queued task.
    let o = Command::new(bin())
        .args(["enqueue", "--root"])
        .arg(&q.root)
        .args(["--json", "--", "-", "-rf"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write;
            c.stdin.as_mut().unwrap().write_all(b"#!/usr/bin/env bash\nexit 0\n")?;
            c.wait_with_output()
        })
        .unwrap();
    assert!(!o.status.success(), "dash slug must not enqueue");
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).expect("rejection is still wire JSON");
    assert_eq!(v["ok"], false);
    // mktask renders without a queue; --slug=-x reaches our check.
    let o = Command::new(bin()).args(["mktask", "--slug=-rf", "--root"]).arg(&q.root).args(["--", "echo", "hi"]).output().unwrap();
    assert!(!o.status.success(), "mktask must reject dash slug: {}", String::from_utf8_lossy(&o.stdout));
    // `new` shares it.
    let o = Command::new(bin()).args(["new", "--slug=-rf", "--root"]).arg(&q.root).args(["--", "echo", "hi"]).output().unwrap();
    assert!(!o.status.success(), "new must reject dash slug");
    assert!(q.names("pending").is_empty(), "rejected slugs must not enqueue: {:?}", q.names("pending"));
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
