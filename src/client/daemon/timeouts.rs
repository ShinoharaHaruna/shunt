//! Wall-clock budget enforcement: TERM on expiry, KILL after the
//! grace window, honest marker bookkeeping (design.md §3.7, §3.9a).

use serde_json::json;
use std::time::Duration;

use super::core::Daemon;
use super::{dlog, ev};
use crate::client::exec;

/// Grace between the timeout TERM and the KILL escalation — same
/// discipline as `shunt kill`.
const TIMEOUT_GRACE: Duration = Duration::from_secs(10);

impl Daemon {
    /// Wall-clock enforcement (design.md §3.7): a task past its
    /// `# shunt:timeout_s=` deadline gets TERM on its verified pgid,
    /// then KILL after the grace window. Both signals go through the
    /// same pid-file + start_time leader check as `kill_task` — the
    /// daemon never signals a group it cannot prove is the task's.
    pub(super) fn enforce_timeouts(&mut self) {
        let now = std::time::Instant::now();
        for (pid, h) in self.runpids.iter_mut() {
            // Leader already reaped: the deadline check must not mark
            // a finished task timed_out while its pgid drains. But a
            // task ALREADY marked timed_out still owes the kill_at
            // escalation — the leader may have died on TERM while
            // detached grandchildren hold the group (§3.9a remnant).
            if h.exited.is_some() && h.timed_out.is_none() {
                continue;
            }
            if h.gone {
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
                // TERM delivered. The §3.9 marker means "killed for
                // exceeding budget": a task that merely outlived its
                // deadline unsignaled must not wear it — a clean rc-0
                // finish then correctly lands in done/, and a
                // non-zero rc in failed/ as a plain failure, not a
                // budget death.
                match exec::read_pid_file(&self.root, "running", &h.base) {
                    Some((pgid, start)) if exec::pgid_is_task_group(pgid, start) => match exec::signal_pgid(pgid, "TERM") {
                        exec::SignalOutcome::Delivered => {
                            h.timed_out = h.dirs.timeout_s;
                            h.kill_at = Some(now + TIMEOUT_GRACE);
                            dlog(&self.root, "task_timeout", ev(json!({"task": h.base, "pgid": pgid, "limit_s": h.timed_out})));
                        }
                        // ESRCH race: the group died on its own
                        // between the gate and the kill. Nothing was
                        // signaled, so nothing is marked — reap
                        // finalizes with the real rc. The `gone`
                        // flag stops this arm re-logging every poll
                        // until then; retrying TERM on a provably
                        // dead group is pointless.
                        exec::SignalOutcome::Gone => {
                            h.gone = true;
                            dlog(&self.root, "task_timeout_gone", ev(json!({"task": h.base, "pgid": pgid})));
                        }
                        // Signal undeliverable but group alive
                        // (EPERM, transient /proc fault): stay
                        // unmarked and retry next poll — the deadline
                        // stays expired, so a clearing fault is
                        // enforced late rather than never.
                        exec::SignalOutcome::Failed(e) => {
                            if h.unenforceable_warned & 0b01 == 0 {
                                h.unenforceable_warned |= 0b01;
                                dlog(
                                    &self.root,
                                    "timeout_unenforceable",
                                    ev(json!({"task": h.base, "pgid": pgid, "error": e.to_string(),
                                       "warning": "deadline passed but TERM undeliverable; budget stays expired and enforcement retries next cycle"})),
                                );
                            }
                        }
                    },
                    // No pid file (the pid_write_failed blind spot) or
                    // the §3.9a gate refused (group fully dead or pgid
                    // recycled): nothing was signaled, so nothing is
                    // marked. A dead group is finalized by reap with
                    // its real rc; a live-but-ungated one retries.
                    _ => {
                        if h.unenforceable_warned & 0b10 == 0 {
                            h.unenforceable_warned |= 0b10;
                            dlog(
                                &self.root,
                                "timeout_unenforceable",
                                ev(json!({"task": h.base, "pgid": pid,
                                   "warning": "deadline passed but the task's group cannot be verified (no pid file or gate refused); budget stays expired and enforcement retries next cycle"})),
                            );
                        }
                    }
                }
            } else if h.kill_at.is_some_and(|k| now >= k) {
                h.kill_at = None;
                // Reached on both live and reaped leaders: §3.9a
                // group gate, same as kill_task's escalation — a
                // leader that died on TERM while its group lives
                // still gets KILL (the live pgid is unrecyclable);
                // only a fully dead group is left alone.
                if let Some((pgid, start)) = exec::read_pid_file(&self.root, "running", &h.base) {
                    if exec::pgid_is_task_group(pgid, start) {
                        exec::signal_pgid(pgid, "KILL");
                        dlog(&self.root, "task_timeout_kill", ev(json!({"task": h.base, "pgid": pgid})));
                    }
                }
            }
        }
    }
}
