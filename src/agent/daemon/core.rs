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
                // Deadline passed: TERM through the same pid-file +
                // §3.9a group gate as kill_task. `timed_out` is
                // stamped ONLY when enforcement actually happened —
                // TERM delivered, or the group provably died on its
                // own (ESRCH race after the gate). The §3.9 marker
                // means "killed for exceeding budget": a task that
                // merely outlived its deadline unsignaled must not
                // wear it — a clean rc-0 finish then correctly lands
                // in done/, and a non-zero rc in failed/ as a plain
                // failure, not a budget death.
                match exec::read_pid_file(&self.root, "running", &h.base) {
                    Some((pgid, start)) if exec::pgid_is_task_group(pgid, start) => match exec::signal_pgid(pgid, "TERM") {
                        exec::SignalOutcome::Delivered | exec::SignalOutcome::Gone => {
                            h.timed_out = h.timeout_s;
                            h.kill_at = Some(now + TIMEOUT_GRACE);
                            dlog(&self.root, "task_timeout", ev(json!({"task": h.base, "pgid": pid, "limit_s": h.timed_out})));
                        }
                        // Signal undeliverable but group alive
                        // (EPERM, transient /proc fault): stay
                        // unmarked and retry next poll — the deadline
                        // stays expired, so a clearing fault is
                        // enforced late rather than never.
                        exec::SignalOutcome::Failed(e) => {
                            dlog(
                                &self.root,
                                "timeout_unenforceable",
                                ev(json!({"task": h.base, "pgid": pgid, "error": e.to_string(),
                                   "warning": "deadline passed but TERM undeliverable; budget stays expired and enforcement retries next cycle"})),
                            );
                        }
                    },
                    // No pid file (the pid_write_failed blind spot) or
                    // the §3.9a gate refused (group fully dead or pgid
                    // recycled): nothing was signaled, so nothing is
                    // marked. A dead group is finalized by reap with
                    // its real rc; a live-but-ungated one retries.
                    _ => {
                        dlog(
                            &self.root,
                            "timeout_unenforceable",
                            ev(json!({"task": h.base, "pgid": pid,
                               "warning": "deadline passed but the task's group cannot be verified (no pid file or gate refused); budget stays expired and enforcement retries next cycle"})),
                        );
                    }
                }
            } else if h.kill_at.is_some_and(|k| now >= k) {
                h.kill_at = None;
                if let Some((pgid, start)) = exec::read_pid_file(&self.root, "running", &h.base) {
                    // §3.9a group gate, same as kill_task's
                    // escalation: a leader that died on TERM while its
                    // group lives still gets KILL (the live pgid is
                    // unrecyclable); only a fully dead group is left.
                    if exec::pgid_is_task_group(pgid, start) {
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
        let candidates = match queue::pending_list(&self.root) {
            Ok(v) => v,
            Err(e) => {
                // Fail closed (§3.11): an unreadable pending/ is not
                // "empty" — it is unknown. Log loudly and skip
                // dispatch this cycle; the idle-shutdown check below
                // reads pending_nonempty, which fails closed too, so
                // a transient fs fault can never masquerade as a
                // drained queue for the poweroff decision.
                dlog(
                    &self.root,
                    "pending_scan_failed",
                    ev(json!({"error": format!("{e:#}"),
                           "warning": "pending/ unreadable; dispatch skipped this cycle and the queue is treated as non-empty"})),
                );
                Vec::new()
            }
        };
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
                    // Without the pid file the task runs unreachable
                    // by `shunt kill` — still run it, but make the
                    // blind spot loud in the log.
                    if let Err(e) = std::fs::write(queue::piece_file(&self.root, "running", &next, "pid"), line) {
                        dlog(
                            &self.root,
                            "pid_write_failed",
                            ev(json!({"task": next, "pgid": pid, "error": e.to_string(),
                               "warning": "task running with no pid file; kill cannot reach it"})),
                        );
                    }
                    let deadline = timeout_s.map(|s| std::time::Instant::now() + Duration::from_secs(s));
                    // Pid-reuse guard (§3.9): a reaped leader's handle
                    // can sit in runpids for up to pgid_drain_grace
                    // while its pid is already free for the OS to
                    // hand to this very spawn. A colliding insert
                    // would silently drop the old handle, leaving its
                    // running/ entry a pid-less ghost — invisible to
                    // kill, counted as running, blocking idle-shutdown.
                    // Collision requires the old leader dead (a live
                    // leader's pid cannot be recycled), but the OS can
                    // hand the pid out in the window between the
                    // leader's exit and reap() observing it — so
                    // exited may still be None. Force-finalize it out
                    // first and log loud.
                    if let Some(mut stale) = self.runpids.remove(&pid) {
                        // If reap() hasn't observed the exit yet, do
                        // it now — dropping Child does not wait — and
                        // recover the real rc rather than fabricate 1.
                        let rc = stale.exited.map(|e| e.rc).or_else(|| stale.child.try_wait().ok().flatten().map(|s| exec::status_rc(&s))).unwrap_or(1);
                        dlog(
                            &self.root,
                            "pid_reused",
                            ev(json!({"task": stale.base, "pgid": pid,
                               "warning": "OS recycled a reaped leader's pid inside the drain grace; force-finalizing the stale entry before tracking the new spawn"})),
                        );
                        if let Err(e) = exec::finalize(&self.root, &stale.base, rc, stale.timed_out) {
                            dlog(&self.root, "finalize_failed", ev(json!({"task": stale.base, "rc": rc, "error": format!("{e:#}")})));
                            let _ = exec::quarantine(&self.root, "running", &stale.base, &format!("finalize failed after pid reuse (rc {rc}): {e:#}"));
                        }
                    }
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
                    // A failed finalize here leaves the same pid-less
                    // ghost §3.9 forbids in the reap path — same
                    // quarantine fallback, same ghost_entry warning.
                    if let Err(e2) = exec::finalize(&self.root, &next, 1, None) {
                        dlog(&self.root, "finalize_failed", ev(json!({"task": next, "rc": 1, "error": format!("{e2:#}")})));
                        if let Err(e3) = exec::quarantine(&self.root, "running", &next, &format!("finalize failed after spawn failure (rc 1): {e2:#}")) {
                            dlog(
                                &self.root,
                                "ghost_entry",
                                ev(json!({"task": next, "error": e3.to_string(),
                                   "warning": "entry stuck in running/ with no pid file; kill cannot reach it"})),
                            );
                        }
                    }
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
        // §3.9a group gate, same as kill_task: signal a pgid still
        // provably naming the task's group — leader-verified, or
        // leader dead but group live. A live pgid cannot be recycled,
        // so a remnant group of detached grandchildren is still ours
        // to kill; only a fully dead group (pgid possibly recycled to
        // a stranger) is skipped.
        let signal_verified = |root: &Path, h: &RunHandle, sig: &str| {
            if let Some((pgid, start)) = exec::read_pid_file(root, "running", &h.base) {
                if exec::pgid_is_task_group(pgid, start) {
                    exec::signal_pgid(pgid, sig);
                }
            }
        };
        for h in self.runpids.values() {
            signal_verified(&self.root, h, "TERM");
        }
        std::thread::sleep(Duration::from_secs(2));
        for h in self.runpids.values() {
            signal_verified(&self.root, h, "KILL");
        }

        // Finalize the killed tasks so they land in failed/ rather
        // than orphaning as running/ entries for the next daemon.
        // Runs long enough to cover pgid_drain_grace: a leader reaped
        // early still waits out its group's drain window (§3.9).
        let deadline = std::time::Instant::now() + pgid_drain_grace() + Duration::from_secs(2);
        while !self.runpids.is_empty() && std::time::Instant::now() < deadline {
            self.reap();
            std::thread::sleep(Duration::from_millis(100));
        }
        if self.root.join("shutdown_on_idle").exists() {
            if shutdown::pending_nonempty(&self.root) {
                // dlog only — daemon stderr is daemon.log, which must
                // stay strict JSONL (§3.1); no eprintln twin.
                dlog(&self.root, "signal_left_shutdown_armed", ev(json!({"warning": "daemon exiting on signal with shutdown_on_idle armed and pending work; a restarted daemon will honor the flag"})));
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
    // Startup mutex (§3.4): an flock on daemon.lock held for the
    // daemon's whole lifetime serializes the alive() check + pidfile
    // write — two concurrent `shunt start`/`shunt daemon` can no
    // longer both pass and spawn twin daemons that fight over the
    // shared pidfile and registry entry. Fail fast on a held lock:
    // it means a live daemon (unlike .seq.lock there is nothing to
    // wait out). flock auto-releases on process death, so a crashed
    // daemon never wedges the next start.
    let _lock = std::fs::OpenOptions::new().create(true).append(true).open(root.join("daemon.lock")).context("cannot open daemon.lock")?;
    match fs2::FileExt::try_lock_exclusive(&_lock) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => anyhow::bail!("daemon already running (daemon.lock held)"),
        Err(e) => return Err(e).context("flock daemon.lock failed"),
    }
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

#[cfg(test)]
mod tests {
    use fs2::FileExt;

    /// Fail closed on an unreadable pending/ (§3.11): `pending` as a
    /// plain file makes pending_list error (ENOTDIR); dispatch must
    /// skip the cycle without spawning, and the event must be in
    /// daemon.log — a silent "empty" would feed idle-shutdown a
    /// phantom drained queue.
    #[test]
    fn dispatch_skips_cycle_on_unreadable_pending() {
        let root = std::env::temp_dir().join(format!("shunt-dispatch-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("pending"), b"not a dir").unwrap();
        let mut d = super::Daemon { root: root.clone(), runpids: Default::default(), last_workers: None, got_signal: Default::default() };
        d.dispatch();
        assert!(d.runpids.is_empty(), "nothing may dispatch off an unreadable pending/");
        let log = std::fs::read_to_string(root.join("daemon.log")).unwrap();
        assert!(log.contains("pending_scan_failed"), "scan failure must be loud in daemon.log");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// §3.7/§3.9 timeout marker honesty: when the deadline has passed
    /// but no TERM can be delivered (here: no pid file — the
    /// pid_write_failed blind spot), `timed_out`/`kill_at` must stay
    /// unset and a `timeout_unenforceable` WARNING logged. The marker
    /// means "killed for exceeding budget"; stamping it on a task
    /// that was never signaled would force a clean rc-0 finish into
    /// failed/ as a fake budget death.
    #[test]
    fn unenforceable_timeout_stays_unmarked() {
        let root = std::env::temp_dir().join(format!("shunt-timeout-test-{}", std::process::id()));
        std::fs::create_dir_all(root.join("running")).unwrap();
        let child = std::process::Command::new("sleep").arg("60").spawn().unwrap();
        let pid = child.id();
        let h = super::RunHandle { base: "t-timeout".into(), child, timeout_s: Some(1), deadline: Some(std::time::Instant::now() - std::time::Duration::from_secs(1)), timed_out: None, kill_at: None, exited: None };
        let mut d = super::Daemon { root: root.clone(), runpids: [(pid, h)].into_iter().collect(), last_workers: None, got_signal: Default::default() };
        d.enforce_timeouts();
        let h = &d.runpids[&pid];
        assert!(h.timed_out.is_none() && h.kill_at.is_none(), "no signal delivered — nothing may be marked");
        let log = std::fs::read_to_string(root.join("daemon.log")).unwrap();
        assert!(log.contains("timeout_unenforceable"), "undeliverable TERM must be loud in daemon.log");
        let child = &mut d.runpids.get_mut(&pid).unwrap().child;
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Startup mutex (§3.4): a held daemon.lock means a live daemon —
    /// run() must bail "already running" immediately rather than
    /// proceeding to the pidfile and spawning a twin. flock releases
    /// on process death, so the failed start leaves nothing behind.
    #[test]
    fn held_daemon_lock_bails_already_running() {
        let root = std::env::temp_dir().join(format!("shunt-dlock-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let lock = std::fs::OpenOptions::new().create(true).append(true).open(root.join("daemon.lock")).unwrap();
        lock.try_lock_exclusive().unwrap();
        let err = super::run(&root).unwrap_err();
        assert!(format!("{err:#}").contains("daemon already running"), "unexpected error: {err:#}");
        drop(lock);
        let _ = std::fs::remove_dir_all(&root);
    }
}
