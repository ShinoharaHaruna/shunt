//! Daemon loop (design.md §3.5–3.10): reap → STOP drain or dispatch →
//! idle-shutdown check, on a 2 s poll (`POLL` env overridable).

use anyhow::{Context, Result};
use serde_json::{json, Map};
use std::collections::HashMap;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
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
        if let Some((pgid, start)) = exec::read_pid_file(root, "running", &base) {
            // A recorded start_time that no longer matches means the
            // pgid was recycled — the leftover group is a stranger's,
            // so no warning (and nothing is ever signaled here).
            let ours = start.is_none_or(|s| exec::proc_pgrp_start(pgid).map(|(_, st)| st == s).unwrap_or(false));
            if ours && exec::pgid_alive(pgid) {
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
            if let Err(e) = std::fs::rename(&dir, pending.join(format!("0000_requeued_{clean}"))) {
                // A failed rename must not strand the entry in
                // running/ forever: no pid file means kill/status
                // can't see it. Quarantine to failed/ so the task
                // stays accounted for (design.md §3.6).
                dlog(root, "requeue_failed", ev(json!({"task": base, "error": e.to_string()})));
                if let Err(e2) = exec::quarantine(root, "running", &base, &format!("orphan requeue failed: {e}")) {
                    dlog(
                        root,
                        "ghost_entry",
                        ev(json!({"task": base, "error": e2.to_string(),
                           "warning": "entry stuck in running/ with no pid file; kill cannot reach it"})),
                    );
                }
                continue;
            }
        } else {
            // Legacy flat entry: stays a flat `NNNN_slug.sh` in
            // pending so the listing keeps seeing it.
            if let Err(e) = std::fs::rename(running.join(format!("{base}.sh")), pending.join(format!("0000_requeued_{clean}.sh"))) {
                dlog(root, "requeue_failed", ev(json!({"task": base, "error": e.to_string()})));
                if let Err(e2) = exec::quarantine(root, "running", &base, &format!("orphan requeue failed: {e}")) {
                    dlog(
                        root,
                        "ghost_entry",
                        ev(json!({"task": base, "error": e2.to_string(),
                           "warning": "entry stuck in running/ with no pid file; kill cannot reach it"})),
                    );
                }
                continue;
            }
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
/// `stop_consumed`: caller already removed the STOP sentinel, so it
/// must not cancel this countdown (STOP drain honoring an armed flag).
/// `abort_at`: `(signal counter, base)` — a further signal past `base`
/// cancels the countdown, so a second TERM during signal-path honoring
/// still kills the daemon promptly.
fn maybe_shutdown(root: &Path, running_empty: bool, stop_consumed: bool, abort_at: Option<(&Arc<AtomicUsize>, usize)>) {
    let flag = root.join("shutdown_on_idle");
    let grace = shutdown::grace(root);
    dlog(root, "idle_shutdown_armed", ev(json!({"grace_s": grace})));
    eprintln!("[shunt] IDLE-SHUTDOWN ARMED: poweroff in {grace}s (cancel: shunt shutdown off / shunt stop / enqueue a task)");
    for i in (1..=grace).rev() {
        std::thread::sleep(Duration::from_secs(1));
        if let Some((count, base)) = abort_at {
            if count.load(Ordering::SeqCst) > base {
                dlog(root, "idle_shutdown_cancelled", ev(json!({"why": "signal during honoring countdown"})));
                return;
            }
        }
        if !flag.exists() {
            dlog(root, "idle_shutdown_cancelled", ev(json!({"why": "flag removed"})));
            return;
        }
        if shutdown::pending_nonempty(root) {
            dlog(root, "idle_shutdown_cancelled", ev(json!({"why": "pending task arrived"})));
            return;
        }
        // A sibling daemon on this box with pending/running work must
        // not be powered off mid-flight. The flag stays armed; the
        // poll loop re-arms the countdown and retries once the box
        // drains (design.md §3.11).
        let peers = shutdown::busy_peers(root);
        if !peers.is_empty() {
            dlog(root, "idle_shutdown_blocked", ev(json!({"peers": peers.iter().map(|p| p.display().to_string()).collect::<Vec<_>>()})));
            return;
        }
        if !stop_consumed && root.join("STOP").exists() {
            // STOP during a countdown is the operator's cancel: disarm
            // too, or the drain-exit path would just re-honor the flag
            // and fire anyway (design.md §3.10/§3.11).
            let _ = std::fs::remove_file(&flag);
            dlog(root, "idle_shutdown_cancelled", ev(json!({"why": "STOP sentinel", "disarmed": true})));
            return;
        }
        if i % 10 == 0 || i <= 5 {
            dlog(root, "idle_shutdown_countdown", ev(json!({"remain_s": i})));
        }
    }
    // final re-check immediately before exec
    let peers = shutdown::busy_peers(root);
    if flag.exists() && !shutdown::pending_nonempty(root) && running_empty && peers.is_empty() {
        let cmd = shutdown::resolve_cmd(root);
        dlog(root, "idle_shutdown_exec", ev(json!({"cmd": cmd})));
        let _ = std::fs::remove_file(root.join("daemon.pid"));
        shutdown::deregister_qroot(root);
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
    /// Count of TERM/INT/HUP seen; >0 means exit at the next boundary.
    got_signal: Arc<AtomicUsize>,
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
            if let Err(e) = exec::finalize(&self.root, &base, rc) {
                // A failed rename leaves a pid-less entry in
                // running/ — invisible to kill and status forever.
                // Quarantine to failed/ so it stays accounted for;
                // if even that fails, scream in the log.
                dlog(&self.root, "finalize_failed", ev(json!({"task": base, "rc": rc, "error": format!("{e:#}")})));
                if let Err(e2) = exec::quarantine(&self.root, "running", &base, &format!("finalize failed (rc {rc}): {e:#}")) {
                    dlog(
                        &self.root,
                        "ghost_entry",
                        ev(json!({"task": base, "error": e2.to_string(),
                           "warning": "entry stuck in running/ with no pid file; kill cannot reach it"})),
                    );
                }
            }
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
        // Snapshot the candidate list once per cycle: iterating the
        // head and `continue`ing on a failed claim would hot-spin on
        // the same unclaimable entry for the whole poll.
        let candidates = queue::pending_list(&self.root).unwrap_or_default();
        for next in candidates {
            if self.runpids.len() >= workers as usize {
                break;
            }
            // claim by mv; a concurrent cancel winning the race is the
            // recheck, not a prior existence test. Dir entries rename
            // whole; legacy flat files are normalized into dirs here.
            match queue::claim(&self.root, &next) {
                Ok(false) => continue, // lost a cancel race; next candidate
                Err(e) => {
                    // The entry still exists but cannot be claimed
                    // (name collision in running/, permissions).
                    // Quarantine it rather than letting it poison
                    // every future poll's head-of-queue.
                    dlog(&self.root, "claim_failed", ev(json!({"task": next, "error": format!("{e:#}")})));
                    if let Err(e2) = exec::quarantine(&self.root, "pending", &next, &format!("claim failed: {e:#}")) {
                        dlog(
                            &self.root,
                            "claim_quarantine_failed",
                            ev(json!({"task": next, "error": e2.to_string(),
                               "warning": "unclaimable pending entry could not be moved to failed/"})),
                        );
                    }
                    continue;
                }
                Ok(true) => {}
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(queue::task_file(&self.root, "running", &next), std::fs::Permissions::from_mode(0o755));
            }
            match exec::spawn_task(&self.root, &next) {
                Ok(child) => {
                    let pid = child.id();
                    // Record the leader's /proc start_time beside the
                    // pgid: kill_task verifies it before signaling so
                    // a recycled pgid can never be hit by mistake.
                    let line = match exec::proc_pgrp_start(pid) {
                        Some((_, start)) => format!("{pid} {start}\n"),
                        None => format!("{pid}\n"),
                    };
                    let _ = std::fs::write(queue::piece_file(&self.root, "running", &next, "pid"), line);
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

    /// TERM/INT/HUP: TERM every tracked pgid, wait 2 s, KILL
    /// survivors, reap them into `failed/`, then exit 143
    /// (design.md §3.5). An armed `shutdown_on_idle` must not be
    /// orphaned here either — the daemon is the only process that can
    /// fire it — so with `pending/` empty it is honored exactly as in
    /// the STOP-drain path (grace countdown, then fire); the pidfile
    /// and registry entry survive the countdown. A further signal
    /// during the countdown cancels it and exits promptly. With
    /// `pending/` non-empty the flag stays armed and a WARNING is
    /// logged: a restarted daemon drains the queue and honors it.
    fn on_term(&mut self) -> ! {
        dlog(&self.root, "signal", ev(json!({"forwarded_to": self.runpids.len()})));
        for pid in self.runpids.keys() {
            exec::signal_pgid(*pid, "TERM");
        }
        std::thread::sleep(Duration::from_secs(2));
        for pid in self.runpids.keys() {
            exec::signal_pgid(*pid, "KILL");
        }
        // Finalize the killed tasks so they land in failed/ rather
        // than orphaning as running/ entries for the next daemon.
        for _ in 0..20 {
            if self.runpids.is_empty() {
                break;
            }
            self.reap();
            std::thread::sleep(Duration::from_millis(100));
        }
        if self.root.join("shutdown_on_idle").exists() {
            if shutdown::pending_nonempty(&self.root) {
                dlog(&self.root, "signal_left_shutdown_armed", ev(json!({"warning": "daemon exiting on signal with shutdown_on_idle armed and pending work; a restarted daemon will honor the flag"})));
                eprintln!("[shunt] WARNING: exiting on signal; shutdown_on_idle still armed with pending tasks — restart the daemon to honor it");
            } else {
                dlog(&self.root, "signal_honors_shutdown", Map::new());
                let base = self.got_signal.load(Ordering::SeqCst);
                maybe_shutdown(&self.root, self.runpids.is_empty(), false, Some((&self.got_signal, base)));
            }
        }
        let _ = std::fs::remove_file(self.root.join("daemon.pid"));
        shutdown::deregister_qroot(&self.root);
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
    shutdown::register_qroot(root);

    let got_signal = Arc::new(AtomicUsize::new(0));
    {
        let count = Arc::clone(&got_signal);
        let mut sigs = signal_hook::iterator::Signals::new(SIGNALS).context("cannot install signal handlers")?;
        std::thread::spawn(move || {
            for _ in sigs.forever() {
                count.fetch_add(1, Ordering::SeqCst);
            }
        });
    }

    let root = root.to_path_buf();
    dlog(&root, "daemon_start", ev(json!({"pid": std::process::id()})));
    requeue_orphans(&root)?;

    let mut d = Daemon { root: root.clone(), runpids: HashMap::new(), last_workers: None, got_signal };
    let poll = poll_interval();
    loop {
        if d.got_signal.load(Ordering::SeqCst) > 0 {
            d.on_term();
        }
        d.reap();
        if d.root.join("STOP").exists() {
            if d.runpids.is_empty() {
                let _ = std::fs::remove_file(d.root.join("STOP"));
                dlog(&d.root, "stop_drained", Map::new());
                // An armed shutdown_on_idle must not be orphaned: the
                // daemon is the only process that can fire it, so honor
                // it now — grace countdown, then power off — instead of
                // exiting and leaving the box billing idle (§3.10/§3.11).
                // STOP is already consumed, so it cannot cancel; flag rm
                // and a new pending entry still can.
                if d.root.join("shutdown_on_idle").exists() && !shutdown::pending_nonempty(&d.root) {
                    dlog(&d.root, "stop_drain_honors_shutdown", Map::new());
                    maybe_shutdown(&d.root, true, true, None);
                }
                let _ = std::fs::remove_file(d.root.join("daemon.pid"));
                shutdown::deregister_qroot(&d.root);
                return Ok(());
            }
            // STOP set while tasks run: no new dispatches, drain only
        } else {
            d.dispatch();
            if d.root.join("shutdown_on_idle").exists() && d.runpids.is_empty() && !shutdown::pending_nonempty(&d.root) {
                maybe_shutdown(&d.root, true, false, None);
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
