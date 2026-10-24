//! Shared fixture for the client-queue integration tests
//!
//! Each test target uses only a subset of these helpers; dead-code
//! warnings would fire per target, so the whole module is allow-listed.
#![allow(dead_code)]
//!
//! (design.md §11): a real `shunt daemon` against a tmp queue root,
//! poll interval shortened via `POLL`. Every wait is
//! condition-based with a timeout; no fixed sleeps are relied on.

pub mod room;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

pub fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_shunt")
}

pub static SEQ: AtomicUsize = AtomicUsize::new(0);

pub struct Q {
    pub root: PathBuf,
}

impl Q {
    pub fn new(tag: &str) -> Self {
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!("shunt-test-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let q = Q { root };
        q.run_ok(&["init"]);
        q
    }

    pub fn run(&self, args: &[&str]) -> Output {
        let mut c = Command::new(bin());
        let mut argv: Vec<String> = vec![args[0].to_string(), "--root".into(), self.root.to_string_lossy().into_owned()];
        argv.extend(args[1..].iter().map(|s| s.to_string()));
        c.args(&argv).env("SHUNT_STATE", self.root.join("boxstate"));
        c.output().expect("shunt run failed")
    }

    pub fn run_ok(&self, args: &[&str]) -> String {
        let o = self.run(args);
        assert!(o.status.success(), "shunt {args:?} failed: {}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).into_owned()
    }

    pub fn run_err(&self, args: &[&str]) -> String {
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
    pub fn daemon(&self) -> Child {
        self.daemon_state(&self.root.join("boxstate"))
    }

    /// Same as `daemon()` but with an explicit shared `SHUNT_STATE` —
    /// two queues on one "box" for peer-coordination tests.
    pub fn daemon_state(&self, state: &Path) -> Child {
        let mut c = Command::new(bin());
        c.args(["daemon", "--root"]).arg(&self.root).env("POLL", "0.1").env("SHUNT_STATE", state).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        c.spawn().expect("spawn daemon")
    }

    /// `shunt start` (self-daemonizing path).
    pub fn start(&self) -> String {
        let mut c = Command::new(bin());
        c.args(["start", "--root"]).arg(&self.root).env("POLL", "0.1").env("SHUNT_STATE", self.root.join("boxstate"));
        let o = c.output().expect("shunt start failed");
        assert!(o.status.success(), "start failed: {}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).into_owned()
    }

    pub fn stop(&self) {
        let _ = self.run(&["stop"]);
    }

    pub fn enqueue_script(&self, slug: &str, body: &str) -> String {
        let f = self.root.join(format!("task-{slug}.sh.src"));
        std::fs::write(&f, body).unwrap();
        self.run_ok(&["enqueue", f.to_str().unwrap(), slug]);
        let mut names: Vec<String> = std::fs::read_dir(self.root.join("pending")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.contains(slug)).collect();
        names.sort();
        names.pop().expect("task not enqueued")
    }

    pub fn names(&self, track: &str) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(self.root.join(track)).map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
        v.sort();
        v
    }

    /// Task entries in a track: dir entries plus legacy `.sh` stems.
    pub fn tasks(&self, track: &str) -> Vec<String> {
        self.names(track).into_iter().filter_map(|n| if self.root.join(track).join(&n).is_dir() { Some(n) } else { n.strip_suffix(".sh").map(|s| s.to_string()) }).collect()
    }

    /// Entries carrying an `exit` file: `(name, rc)`.
    pub fn exits(&self, track: &str) -> Vec<(String, i32)> {
        self.tasks(track)
            .into_iter()
            .filter_map(|n| {
                let ef = if self.root.join(track).join(&n).is_dir() { self.root.join(track).join(&n).join("exit") } else { self.root.join(track).join(format!("{n}.sh.exit")) };
                std::fs::read_to_string(&ef).ok()?.trim().parse::<i32>().ok().map(|rc| (n, rc))
            })
            .collect()
    }

    pub fn daemon_pid(&self) -> Option<u32> {
        std::fs::read_to_string(self.root.join("daemon.pid")).ok().and_then(|s| s.trim().parse().ok())
    }

    pub fn dlog(&self) -> String {
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

pub fn wait_until(mut cond: impl FnMut() -> bool, secs: u64, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for: {what}");
}

pub fn sleep_task() -> String {
    "#!/usr/bin/env bash\nexec sleep 30\n".to_string()
}

pub fn marker_task(marker: &Path) -> String {
    format!("#!/usr/bin/env bash\ntouch {}\n", marker.display())
}
