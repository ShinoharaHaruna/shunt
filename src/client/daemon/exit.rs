//! Signal exit path: TERM/INT/HUP drains the runpids, then honors
//! or loudly orphans an armed shutdown_on_idle (design.md §3.5).

use serde_json::{json, Map};
use std::path::Path;
use std::time::Duration;

use super::core::{pgid_drain_grace, Daemon, RunHandle};
use super::idle::maybe_shutdown;
use super::{dlog, ev};
use crate::client::{exec, shutdown};

/// §3.5 loud-orphan check for the signal-exit path: an armed
/// `shutdown_on_idle` with pending work must be logged before exit —
/// the daemon is the only process that can fire it, so a silent
/// orphan would leave the box billing idle with queued work. A
/// removed flag (clean disarm during the countdown) or a busy-peer
/// block (`idle_shutdown_blocked` — a live sibling can still honor
/// the flag) is not an orphan and logs nothing. dlog only: daemon
/// stderr is daemon.log, which must stay strict JSONL (§3.1).
pub(super) fn warn_orphaned_shutdown_flag(root: &Path) {
    if root.join("shutdown_on_idle").exists() && shutdown::pending_nonempty(root) {
        dlog(root, "signal_left_shutdown_armed", ev(json!({"warning": "daemon exiting on signal with shutdown_on_idle armed and pending work; a restarted daemon will honor the flag"})));
    }
}

impl Daemon {
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
    pub(super) fn on_term(&mut self) -> ! {
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
                warn_orphaned_shutdown_flag(&self.root);
            } else {
                dlog(&self.root, "signal_honors_shutdown", Map::new());
                maybe_shutdown(&self.root, self.runpids.is_empty(), false, Some(&self.got_signal));
                // §3.5: a countdown cancelled by new pending work
                // returns here with the flag still armed — the exit
                // must carry the same loud-orphan WARNING as the
                // pre-check branch, not fall through silently.
                warn_orphaned_shutdown_flag(&self.root);
            }
        }
        let _ = std::fs::remove_file(self.root.join("daemon.pid"));
        shutdown::deregister_qroot(&self.root);
        std::process::exit(143);
    }
}
