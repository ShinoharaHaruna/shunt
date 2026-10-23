//! Claim/dispatch: worker-pool resolution, retry_after gating,
//! claim → spawn → runpid tracking (design.md §3.5, §3.8, §3.9).

use serde_json::json;
use std::time::Duration;

use super::core::{Daemon, RunHandle};
use super::workers::desired_workers_in;
use super::{dlog, ev};
use crate::client::{exec, queue};

impl Daemon {
    pub(super) fn dispatch(&mut self) {
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
        let now = crate::proto::time::unix_now();
        for next in candidates {
            if self.runpids.len() >= workers as usize {
                break;
            }
            // A rescheduled entry waits out its `retry_after` delay
            // (§3.9): not yet due -> skip it this cycle. A garbage
            // sidecar can't wedge the entry — unparseable = due now,
            // and the file is cleared on the way through.
            let ra = queue::piece_file(&self.root, "pending", &next, "retry_after");
            if ra.exists() {
                let due = std::fs::read_to_string(&ra).ok().and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(0);
                if due > now {
                    continue;
                }
                let _ = std::fs::remove_file(&ra);
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
                Ok((child, dirs)) => {
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
                    // `spawned_at` lets a §3.5a handoff reconstruct the
                    // timeout deadline from the ORIGINAL spawn instead
                    // of re-timing it — a restart must never stretch a
                    // task's budget (§3.13). Dir entries only; flat
                    // legacy entries carry no sidecars.
                    if queue::is_dir_entry(&self.root, "running", &next) {
                        let _ = std::fs::write(queue::piece_file(&self.root, "running", &next, "spawned_at"), format!("{now}\n"));
                    }
                    let deadline = dirs.timeout_s.map(|s| std::time::Instant::now() + Duration::from_secs(s));
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
                    if let Some(stale) = self.runpids.remove(&pid) {
                        // If reap() hasn't observed the exit yet, do
                        // it now — and recover the real rc rather
                        // than fabricate 1.
                        let rc = stale
                            .exited
                            .map(|e| e.rc)
                            .or_else(|| match exec::poll_leader(pid) {
                                exec::LeaderPoll::Exited(rc) => Some(rc),
                                _ => None,
                            })
                            .unwrap_or(1);
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
                    let timeout_s = dirs.timeout_s;
                    self.runpids.insert(pid, RunHandle { base: next.clone(), dirs, deadline, usage: Default::default(), last_jiffies: 0, timed_out: None, kill_at: None, gone: false, unenforceable_warned: 0, exited: None });
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
}
