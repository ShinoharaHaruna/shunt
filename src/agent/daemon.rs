//! Daemon loop (design.md §3.5–3.10): reap → STOP drain or dispatch →
//! idle-shutdown check, on a 2 s poll (`POLL` env overridable).

use anyhow::{Context, Result};
use serde_json::{json, Map};
use std::collections::HashMap;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::agent::{exec, queue, shutdown};

const SIGNALS: [i32; 3] = [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT, signal_hook::consts::SIGHUP];

/// Daemon liveness: pidfile + `/proc/<pid>` + cmdline mentions
/// `queue::DAEMON_IDENTITY`. Never pgrep (design.md §3.4). Stale
/// pidfiles are removed.
pub fn alive(root: &Path) -> Result<Option<u32>> {
    let pidf = root.join("daemon.pid");
    let pid: u32 = match std::fs::read_to_string(&pidf).ok().and_then(|s| s.trim().parse().ok()) {
        Some(p) => p,
        None => return Ok(None),
    };
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok();
    let hit = cmdline
        .map(|c| {
            let s = String::from_utf8_lossy(&c).replace('\0', " ");
            s.contains(queue::DAEMON_IDENTITY)
        })
        .unwrap_or(false);
    if hit {
        Ok(Some(pid))
    } else {
        // stale or foreign pidfile
        let _ = std::fs::remove_file(&pidf);
        Ok(None)
    }
}

/// Event fields helper: json! object -> Map for dlog.
fn ev(v: serde_json::Value) -> Map<String, serde_json::Value> {
    v.as_object().cloned().unwrap_or_default()
}

/// Append one JSONL event to daemon.log: `{ts, event, ...}`.
fn dlog(root: &Path, event: &str, extra: Map<String, serde_json::Value>) {
    let mut obj = Map::new();
    obj.insert("ts".into(), json!(queue::iso8601_now()));
    obj.insert("event".into(), json!(event));
    obj.extend(extra);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(root.join("daemon.log")) {
        let _ = writeln!(f, "{}", serde_json::Value::Object(obj));
    }
}

/// Poll interval: `POLL` env (fractional seconds ok) or 2 s.
fn poll_interval() -> Duration {
    std::env::var("POLL").ok().and_then(|s| s.parse::<f64>().ok()).filter(|p| *p > 0.0).map(Duration::from_secs_f64).unwrap_or_else(|| Duration::from_secs(2))
}

/// Desired pool size: `workers` file > `QUEUE_WORKERS` env >
/// `last_known` > default; digits extracted, clamped ≥ 1
/// (design.md §3.8).
fn desired_workers_in(root: &Path, last: Option<u32>) -> u32 {
    let w = if let Ok(s) = std::fs::read_to_string(root.join("workers")) {
        queue::digits_only(&s)
    } else if let Ok(e) = std::env::var("QUEUE_WORKERS") {
        e.trim().parse::<u32>().ok()
    } else {
        Some(queue::DEFAULT_WORKERS)
    };

    match w {
        Some(v) => v.max(1),
        None => last.unwrap_or(queue::DEFAULT_WORKERS).max(1),
    }
}

/// Resolved pool size + source for `status` / `workers get`
/// (never a hardcoded literal — bash defect fix, design.md §3.14).
pub fn resolve_workers(root: &Path) -> crate::proto::wire::WorkersState {
    if let Ok(s) = std::fs::read_to_string(root.join("workers")) {
        if let Some(v) = queue::digits_only(&s) {
            return crate::proto::wire::WorkersState { value: v.max(1), source: "file".into() };
        }
    }
    if let Ok(e) = std::env::var("QUEUE_WORKERS") {
        if let Ok(v) = e.trim().parse::<u32>() {
            return crate::proto::wire::WorkersState { value: v.max(1), source: "env".into() };
        }
    }
    crate::proto::wire::WorkersState { value: queue::DEFAULT_WORKERS, source: "default".into() }
}

/// Orphan requeue: entries left in running/ by a dead daemon go to
/// `pending/0000_requeued_<clean>`; the interrupted log is swept to
/// `failed/<clean>.interrupted.<ts>.log` (design.md §3.6). Dir and
/// legacy flat entries are both handled.
fn requeue_orphans(root: &Path) -> Result<()> {
    let running = root.join("running");
    let pending = root.join("pending");
    let failed = root.join("failed");
    for base in queue::running_list(root)? {
        let pidf = queue::piece_file(root, "running", &base, "pid");
        if let Ok(pgid) = std::fs::read_to_string(&pidf).map(|s| s.trim().parse::<u32>()).and_then(|r| r.map_err(|_| std::io::Error::other("bad pid"))) {
            if exec::pgid_alive(pgid) {
                dlog(
                    root,
                    "orphan_live_pgid",
                    ev(json!({"task": base, "pgid": pgid,
                           "warning": "process group still alive; requeued anyway, rerun should be a no-op via the task guard"})),
                );
            }
        }
        let mut clean = base.as_str();
        while let Some(rest) = clean.strip_prefix("0000_requeued_") {
            clean = rest;
        }
        let interrupted = failed.join(format!("{clean}.interrupted.{}", queue::unix_now()));
        if queue::is_dir_entry(root, "running", &base) {
            let dir = running.join(&base);
            let old_log = dir.join("log");
            if old_log.exists() {
                let _ = std::fs::rename(&old_log, interrupted.with_extension("log"));
            }
            let _ = std::fs::remove_file(dir.join("pid"));
            let _ = std::fs::rename(&dir, pending.join(format!("0000_requeued_{clean}")));
        } else {
            // Legacy flat entry: stays a flat `NNNN_slug.sh` in
            // pending so the listing keeps seeing it.
            let _ = std::fs::rename(running.join(format!("{base}.sh")), pending.join(format!("0000_requeued_{clean}.sh")));
            let old_log = running.join(format!("{base}.sh.log"));
            if old_log.exists() {
                let _ = std::fs::rename(&old_log, failed.join(format!("{clean}.interrupted.{}.log", queue::unix_now())));
            }
            let _ = std::fs::remove_file(running.join(format!("{base}.sh.pid")));
        }
        dlog(root, "requeue_orphan", ev(json!({"from": base, "to": format!("0000_requeued_{clean}")})));
    }
    // sweep legacy strays: .pid/.sh.log files with no entry left
    if let Ok(rd) = std::fs::read_dir(&running) {
        for ent in rd.flatten() {
            let name = ent.file_name().to_string_lossy().into_owned();
            if name.ends_with(".pid") {
                let _ = std::fs::remove_file(ent.path());
            } else if name.ends_with(".log") {
                let clean = name.trim_end_matches(".sh.log").to_string();
                let _ = std::fs::rename(ent.path(), failed.join(format!("{clean}.interrupted.{}.log", queue::unix_now())));
                dlog(root, "sweep_stray_log", ev(json!({"from": name})));
            }
        }
    }
    Ok(())
}

/// Idle-shutdown countdown (design.md §3.11). Returns when cancelled;
/// on success it execs the shutdown command and never returns.
fn maybe_shutdown(root: &Path, running_empty: bool) {
    let flag = root.join("shutdown_on_idle");
    let grace = shutdown::grace(root);
    dlog(root, "idle_shutdown_armed", ev(json!({"grace_s": grace})));
    eprintln!("[shunt] IDLE-SHUTDOWN ARMED: poweroff in {grace}s (cancel: shunt shutdown off / shunt stop / enqueue a task)");
    for i in (1..=grace).rev() {
        std::thread::sleep(Duration::from_secs(1));
        if !flag.exists() {
            dlog(root, "idle_shutdown_cancelled", ev(json!({"why": "flag removed"})));
            return;
        }
        if shutdown::pending_nonempty(root) {
            dlog(root, "idle_shutdown_cancelled", ev(json!({"why": "pending task arrived"})));
            return;
        }
        if root.join("STOP").exists() {
            dlog(root, "idle_shutdown_cancelled", ev(json!({"why": "STOP sentinel"})));
            return;
        }
        if i % 10 == 0 || i <= 5 {
            dlog(root, "idle_shutdown_countdown", ev(json!({"remain_s": i})));
        }
    }
    // final re-check immediately before exec
    if flag.exists() && !shutdown::pending_nonempty(root) && running_empty {
        let cmd = shutdown::resolve_cmd(root);
        dlog(root, "idle_shutdown_exec", ev(json!({"cmd": cmd})));
        let _ = std::fs::remove_file(root.join("daemon.pid"));
        let err = Command::new("sh").arg("-c").arg(&cmd).exec();
        dlog(root, "idle_shutdown_exec_failed", ev(json!({"error": err.to_string()})));
    } else {
        dlog(root, "idle_shutdown_aborted", Map::new());
    }
}

struct Daemon {
    root: PathBuf,
    /// leader pid (= task pgid) -> (task basename, child)
    runpids: HashMap<u32, (String, Child)>,
    last_workers: Option<u32>,
    got_signal: Arc<AtomicBool>,
}

impl Daemon {
    fn reap(&mut self) {
        let mut done = Vec::new();
        for (pid, (base, child)) in self.runpids.iter_mut() {
            match child.try_wait() {
                Ok(Some(status)) => done.push((*pid, base.clone(), exec::status_rc(&status))),
                Ok(None) => {}
                Err(_) => done.push((*pid, base.clone(), 1)),
            }
        }
        for (pid, base, rc) in done {
            self.runpids.remove(&pid);
            let _ = exec::finalize(&self.root, &base, rc);
            dlog(
                &self.root,
                "finish",
                ev(json!({"task": base, "rc": rc,
                       "dest": if rc == 0 { "done" } else { "failed" }})),
            );
        }
    }

    fn dispatch(&mut self) {
        let workers = desired_workers_in(&self.root, self.last_workers);
        if Some(workers) != self.last_workers {
            dlog(&self.root, "workers_now", ev(json!({"workers": workers})));
            self.last_workers = Some(workers);
        }
        while self.runpids.len() < workers as usize {
            let next = match queue::pending_list(&self.root).ok().and_then(|v| v.into_iter().next()) {
                Some(n) => n,
                None => break,
            };
            // claim by mv; a concurrent cancel winning the race is the
            // recheck, not a prior existence test. Dir entries rename
            // whole; legacy flat files are normalized into dirs here.
            if !queue::claim(&self.root, &next) {
                continue;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(queue::task_file(&self.root, "running", &next), std::fs::Permissions::from_mode(0o755));
            }
            match exec::spawn_task(&self.root, &next) {
                Ok(child) => {
                    let pid = child.id();
                    let _ = std::fs::write(queue::piece_file(&self.root, "running", &next, "pid"), format!("{pid}\n"));
                    self.runpids.insert(pid, (next.clone(), child));
                    dlog(
                        &self.root,
                        "start",
                        ev(json!({"task": next, "pgid": pid,
                               "slots": format!("{}/{}", self.runpids.len(), workers)})),
                    );
                }
                Err(e) => {
                    dlog(&self.root, "spawn_failed", ev(json!({"task": next, "error": e.to_string()})));
                    let _ = exec::finalize(&self.root, &next, 1);
                }
            }
        }
    }

    /// TERM/INT/HUP: drop the pidfile, TERM every tracked pgid, wait
    /// 2 s, KILL survivors, exit 143 (design.md §3.5).
    fn on_term(&self) -> ! {
        dlog(&self.root, "signal", ev(json!({"forwarded_to": self.runpids.len()})));
        let _ = std::fs::remove_file(self.root.join("daemon.pid"));
        for pid in self.runpids.keys() {
            exec::signal_pgid(*pid, "TERM");
        }
        std::thread::sleep(Duration::from_secs(2));
        for pid in self.runpids.keys() {
            exec::signal_pgid(*pid, "KILL");
        }
        std::process::exit(143);
    }
}

/// Run the queue loop in the foreground.
pub fn run(root: &Path) -> Result<()> {
    queue::init(root)?;
    if let Some(pid) = alive(root)? {
        anyhow::bail!("daemon already running (pid {pid})");
    }
    std::fs::write(root.join("daemon.pid"), format!("{}\n", std::process::id()))?;

    let got_signal = Arc::new(AtomicBool::new(false));
    {
        let flag = Arc::clone(&got_signal);
        let mut sigs = signal_hook::iterator::Signals::new(SIGNALS).context("cannot install signal handlers")?;
        std::thread::spawn(move || {
            for _ in sigs.forever() {
                flag.store(true, Ordering::SeqCst);
            }
        });
    }

    let root = root.to_path_buf();
    dlog(&root, "daemon_start", ev(json!({"pid": std::process::id()})));
    requeue_orphans(&root)?;

    let mut d = Daemon { root: root.clone(), runpids: HashMap::new(), last_workers: None, got_signal };
    let poll = poll_interval();
    loop {
        if d.got_signal.load(Ordering::SeqCst) {
            d.on_term();
        }
        d.reap();
        if d.root.join("STOP").exists() {
            if d.runpids.is_empty() {
                let _ = std::fs::remove_file(d.root.join("STOP"));
                let _ = std::fs::remove_file(d.root.join("daemon.pid"));
                dlog(&d.root, "stop_drained", Map::new());
                return Ok(());
            }
            // STOP set while tasks run: no new dispatches, drain only
        } else {
            d.dispatch();
            if d.root.join("shutdown_on_idle").exists() && d.runpids.is_empty() && !shutdown::pending_nonempty(&d.root) {
                maybe_shutdown(&d.root, true);
            }
        }
        std::thread::sleep(poll);
    }
}

/// Self-daemonize: spawn `shunt daemon --root <root>` detached
/// (process_group(0), stdio → daemon.log) and wait for the pidfile.
/// Idempotent: returns the live pid when already running.
pub fn start(root: &Path) -> Result<u32> {
    if let Some(pid) = alive(root)? {
        return Ok(pid);
    }
    queue::init(root)?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(root.join("daemon.log"))?;
    let err = log.try_clone()?;
    let exe = std::env::current_exe()?;
    let mut child = Command::new(exe);
    child.arg("daemon").arg("--root").arg(root).stdin(Stdio::null()).stdout(log).stderr(err).process_group(0);
    let mut spawned = child.spawn().context("cannot spawn daemon")?;
    drop(spawned.stdin.take());
    // the daemon writes its own pidfile; wait until liveness confirms
    for _ in 0..100 {
        if let Some(pid) = alive(root)? {
            return Ok(pid);
        }
        if let Ok(Some(_)) = spawned.try_wait() {
            anyhow::bail!("daemon exited during startup; check {}", root.join("daemon.log").display());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    anyhow::bail!("daemon did not come up within 5 s")
}

/// Signal the daemon to drain and exit: place the STOP sentinel.
pub fn stop(root: &Path) -> Result<()> {
    std::fs::write(root.join("STOP"), b"")?;
    Ok(())
}
