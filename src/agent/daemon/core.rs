//! Poll loop, in-flight task bookkeeping, claim/dispatch, reap,
//! timeout enforcement, and the signal exit path (design.md §3.5–3.10).

use anyhow::{Context, Result};
use serde_json::{json, Map};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::idle::maybe_shutdown;
use super::lifecycle::alive;
use super::reap::requeue_orphans;
use super::workers::desired_workers_in;
use super::{dlog, ev, poll_interval};
use crate::agent::{exec, queue, shutdown};

const SIGNALS: [i32; 3] = [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT, signal_hook::consts::SIGHUP];

/// Grace between the timeout TERM and the KILL escalation — same
/// discipline as `shunt kill`.
const TIMEOUT_GRACE: Duration = Duration::from_secs(10);

/// After the leader exits, finalize waits this long for the rest of
/// the process group to drain before moving the entry anyway
/// (design.md §3.9). `SHUNT_PGID_DRAIN_S` overrides (tests).
fn pgid_drain_grace() -> Duration {
    std::env::var("SHUNT_PGID_DRAIN_S").ok().and_then(|s| s.parse::<u64>().ok()).map(Duration::from_secs).unwrap_or(Duration::from_secs(5))
}

/// Leader reaped but the process group may still hold grandchildren:
/// finalize is deferred until the pgid drains or the bounded grace
/// expires (design.md §3.9).
struct Exited {
    rc: i32,
    deadline: std::time::Instant,
}

/// One in-flight task: the spawned leader plus its wall-clock budget
/// bookkeeping. `deadline` is spawn-time + `# shunt:timeout_s=`;
/// `timed_out` is set once the budget is exceeded so reap can mark
/// the finalize; `kill_at` is the TERM grace expiry for escalation.
struct RunHandle {
    base: String,
    child: Child,
    /// Wall-clock budget seconds from `# shunt:timeout_s=` (None = unlimited).
    timeout_s: Option<u64>,
    deadline: Option<std::time::Instant>,
    timed_out: Option<u64>,
    kill_at: Option<std::time::Instant>,
    /// Set once the leader is reaped; finalize waits out the pgid.
    exited: Option<Exited>,
}

struct Daemon {
    root: PathBuf,
    /// leader pid (= task pgid) -> handle
    runpids: HashMap<u32, RunHandle>,
    last_workers: Option<u32>,
    /// Count of TERM/INT/HUP seen; >0 means exit at the next boundary.
    got_signal: Arc<AtomicUsize>,
}

impl Daemon {
    /// Wall-clock enforcement (design.md §3.7): a task past its
    /// `# shunt:timeout_s=` deadline gets TERM on its verified pgid,
    /// then KILL after the grace window. Both signals go through the
    /// same pid-file + start_time leader check as `kill_task` — the
    /// daemon never signals a group it cannot prove is the task's.
    fn enforce_timeouts(&mut self) {
        let now = std::time::Instant::now();
        for (pid, h) in self.runpids.iter_mut() {
            // Leader already reaped: the deadline check must not mark
            // a finished task timed_out while its pgid drains.
            if h.exited.is_some() {
                continue;
            }
            if h.timed_out.is_none() {
                let expired = h.deadline.is_some_and(|d| now >= d);
                if !expired {
                    continue;
                }
                // Deadline passed: the task exceeded its budget
                // regardless of how it dies from here.
                if let Some((pgid, start)) = exec::read_pid_file(&self.root, "running", &h.base) {
                    if exec::pgid_is_task_leader(pgid, start) {
                        exec::signal_pgid(pgid, "TERM");
                    }
                }
                h.timed_out = h.timeout_s;
                h.kill_at = Some(now + TIMEOUT_GRACE);
                dlog(&self.root, "task_timeout", ev(json!({"task": h.base, "pgid": pid, "limit_s": h.timed_out})));
            } else if h.kill_at.is_some_and(|k| now >= k) {
                h.kill_at = None;
                if let Some((pgid, start)) = exec::read_pid_file(&self.root, "running", &h.base) {
                    if exec::pgid_is_task_leader(pgid, start) {
                        exec::signal_pgid(pgid, "KILL");
                        dlog(&self.root, "task_timeout_kill", ev(json!({"task": h.base, "pgid": pgid})));
                    }
                }
            }
        }
    }

    fn reap(&mut self) {
        let grace = pgid_drain_grace();
        let now = std::time::Instant::now();
        let mut done = Vec::new();
        for (pid, h) in self.runpids.iter_mut() {
            match &mut h.exited {
                // Leader already reaped: finalize once the process
                // group is fully gone, or once the bounded grace
                // expires — surviving grandchildren keep writing to
                // the moved log but the entry can wait no longer.
                Some(ex) => {
                    if !exec::pgid_alive(*pid) {
                        done.push((*pid, h.base.clone(), ex.rc, false));
                    } else if now >= ex.deadline {
                        done.push((*pid, h.base.clone(), ex.rc, true));
                    }
                }
                None => match h.child.try_wait() {
                    Ok(Some(status)) => {
                        if exec::pgid_alive(*pid) {
                            h.exited = Some(Exited { rc: exec::status_rc(&status), deadline: now + grace });
                        } else {
                            done.push((*pid, h.base.clone(), exec::status_rc(&status), false));
                        }
                    }
                    Ok(None) => {}
                    Err(_) => {
                        if exec::pgid_alive(*pid) {
                            h.exited = Some(Exited { rc: 1, deadline: now + grace });
                        } else {
                            done.push((*pid, h.base.clone(), 1, false));
                        }
                    }
                },
            }
        }
        for (pid, base, rc, lingering) in done {
            let timed_out = self.runpids.get(&pid).and_then(|h| h.timed_out);
            self.runpids.remove(&pid);
            if lingering {
                dlog(
                    &self.root,
                    "finalize_lingering_pgid",
                    ev(json!({"task": base, "pgid": pid,
                       "warning": "task leader exited but the process group is still alive; finalized anyway — survivors keep writing to the moved log and are unreachable by kill"})),
                );
            }
            if let Err(e) = exec::finalize(&self.root, &base, rc, timed_out) {
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
                       "timeout": timed_out,
                       "dest": if rc == 0 && timed_out.is_none() { "done" } else { "failed" }})),
            );
        }
    }

    fn dispatch(&mut self) {
        let workers = desired_workers_in(&self.root, self.last_workers);
        if Some(workers) != self.last_workers {
            dlog(&self.root, "workers_now", ev(json!({"workers": workers})));
            // Persist the resolved value so one-shot `status`/`workers
            // get` fall back to the same last-known the daemon runs
            // (design.md §3.8).
            super::workers::persist_last(&self.root, workers);
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
                Ok((child, timeout_s)) => {
                    let pid = child.id();
                    // Record the leader's /proc start_time beside the
                    // pgid: kill_task verifies it before signaling so
                    // a recycled pgid can never be hit by mistake.
                    let line = match exec::proc_pgrp_start(pid) {
                        Some((_, start)) => format!("{pid} {start}\n"),
                        None => format!("{pid}\n"),
                    };
                    let _ = std::fs::write(queue::piece_file(&self.root, "running", &next, "pid"), line);
                    let deadline = timeout_s.map(|s| std::time::Instant::now() + Duration::from_secs(s));
                    self.runpids.insert(pid, RunHandle { base: next.clone(), child, timeout_s, deadline, timed_out: None, kill_at: None, exited: None });
                    dlog(
                        &self.root,
                        "start",
                        ev(json!({"task": next, "pgid": pid,
                               "timeout_s": timeout_s,
                               "slots": format!("{}/{}", self.runpids.len(), workers)})),
                    );
                }
                Err(e) => {
                    dlog(&self.root, "spawn_failed", ev(json!({"task": next, "error": e.to_string()})));
                    let _ = exec::finalize(&self.root, &next, 1, None);
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
                maybe_shutdown(&self.root, self.runpids.is_empty(), false, Some(&self.got_signal));
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

    // The registry entry is removed on every exit path (§3.11): an
    // error anywhere after registration must not leave the pidfile
    // and box-level entry behind for lazy sweeping.
    let result = run_registered(root);
    if result.is_err() {
        let _ = std::fs::remove_file(root.join("daemon.pid"));
        shutdown::deregister_qroot(root);
    }
    result
}

/// Post-registration half of `run()`: signal handlers, orphan
/// requeue, and the poll loop. Kept separate so `run()` can strip
/// the pidfile/registry entry on any early error.
fn run_registered(root: &Path) -> Result<()> {
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

    // Seed last-known from the previous daemon's persisted value:
    // a garbage `workers` file falls back to it rather than default,
    // matching what a concurrent `status` reports (design.md §3.8).
    let mut d = Daemon { root: root.clone(), runpids: HashMap::new(), last_workers: super::workers::read_last(&root), got_signal };
    let poll = poll_interval();
    loop {
        if d.got_signal.load(Ordering::SeqCst) > 0 {
            d.on_term();
        }
        // reap before enforce: a leader that just exited must be
        // marked `exited` before the deadline check runs, or a
        // clean rc-0 finish at the deadline boundary would still be
        // stamped timed_out and land in failed/ (§3.7).
        d.reap();
        d.enforce_timeouts();
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
                    maybe_shutdown(&d.root, true, true, Some(&d.got_signal));
                }
                let _ = std::fs::remove_file(d.root.join("daemon.pid"));
                shutdown::deregister_qroot(&d.root);
                return Ok(());
            }
            // STOP set while tasks run: no new dispatches, drain only
        } else {
            d.dispatch();
            if d.root.join("shutdown_on_idle").exists() && d.runpids.is_empty() && !shutdown::pending_nonempty(&d.root) {
                maybe_shutdown(&d.root, true, false, Some(&d.got_signal));
            }
        }
        std::thread::sleep(poll);
    }
}
