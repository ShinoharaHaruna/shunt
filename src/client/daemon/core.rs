//! Daemon state and the poll loop: runpids bookkeeping, `run`, and
//! the per-cycle sequence (design.md §3.5–3.10). The Daemon methods
//! live beside their domains: `dispatch`, `timeouts` (enforce),
//! `reap` (leader exits), `exit` (signal path).

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
use super::{dlog, ev, poll_interval};
use crate::client::{exec, queue, shutdown};

const SIGNALS: [i32; 3] = [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT, signal_hook::consts::SIGHUP];

/// After the leader exits, finalize waits this long for the rest of
/// the process group to drain before moving the entry anyway
/// (design.md §3.9). `SHUNT_PGID_DRAIN_S` overrides (tests).
pub(super) fn pgid_drain_grace() -> Duration {
    std::env::var("SHUNT_PGID_DRAIN_S").ok().and_then(|s| s.parse::<u64>().ok()).map(Duration::from_secs).unwrap_or(Duration::from_secs(5))
}

/// Leader reaped but the process group may still hold grandchildren:
/// finalize is deferred until the pgid drains or the bounded grace
/// expires (design.md §3.9).
pub(super) struct Exited {
    pub(super) rc: i32,
    pub(super) deadline: std::time::Instant,
}

/// One in-flight task: the spawned leader plus its wall-clock budget
/// bookkeeping. `deadline` is spawn-time + `# shunt:timeout_s=`;
/// `timed_out` is set once the budget is exceeded so reap can mark
/// the finalize; `kill_at` is the TERM grace expiry for escalation.
pub(super) struct RunHandle {
    pub(super) base: String,
    pub(super) child: Child,
    /// Directives parsed from the task head at spawn — carries the
    /// wall-clock budget AND the reschedule budget (design.md §3.9)
    /// so neither is re-read from disk at reap.
    pub(super) dirs: exec::TaskDirectives,
    pub(super) deadline: Option<std::time::Instant>,
    /// Usage peaks folded in every poll; the file inside the entry
    /// is what `status` reads (design.md §3.16).
    pub(super) usage: crate::client::usage::TaskUsage,
    /// Σ group jiffies at the previous sample — cpu_seconds accrues
    /// only the delta since then.
    pub(super) last_jiffies: u64,
    pub(super) timed_out: Option<u64>,
    pub(super) kill_at: Option<std::time::Instant>,
    /// ESRCH race: the §3.9a gate passed but TERM found the group
    /// already dead. Logged once, then left to reap — never marked
    /// timed_out, since nothing was signaled.
    pub(super) gone: bool,
    /// Once-guard bitmask for `timeout_unenforceable`: bit 0 = TERM
    /// undeliverable (Failed), bit 1 = group unverifiable (no pid
    /// file / gate refused). Without it an unenforceable deadline
    /// would re-log the WARNING every poll (~2 s) for as long as
    /// the condition persists. Each arm logs its own text once;
    /// a later Delivered (task_timeout) or Gone (task_timeout_gone)
    /// ends the condition on its own, so the bits never reset.
    pub(super) unenforceable_warned: u8,
    /// Set once the leader is reaped; finalize waits out the pgid.
    pub(super) exited: Option<Exited>,
}

pub(super) struct Daemon {
    pub(super) root: PathBuf,
    /// leader pid (= task pgid) -> handle
    pub(super) runpids: HashMap<u32, RunHandle>,
    pub(super) last_workers: Option<u32>,
    /// Count of TERM/INT/HUP seen; >0 means exit at the next boundary.
    pub(super) got_signal: Arc<AtomicUsize>,
}

impl Daemon {
    /// Fold one usage reading into every tracked task's peaks and
    /// persist the `usage` sidecar (design.md §3.16). One Snapshot
    /// per cycle — the /proc + nvidia-smi cost is paid once even
    /// with a full worker pool.
    fn sample_usage(&mut self) {
        if self.runpids.is_empty() {
            return;
        }
        let snap = crate::client::usage::Snapshot::take();
        for (pgid, h) in self.runpids.iter_mut() {
            snap.apply(*pgid, &mut h.usage, &mut h.last_jiffies);
            crate::client::usage::write_usage(&self.root, "running", &h.base, &h.usage);
        }
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
    if let Err(error) = shutdown::try_register_qroot(root) {
        shutdown::registration_warning(root, &error);
        let _ = std::fs::remove_file(root.join("daemon.pid"));
        return Err(error).context("repair the qroot registry path/permissions and restart the daemon");
    }

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
        d.sample_usage();
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
            // The own-queue idle gate is `runpids`, not a running/
            // listing — deliberately asymmetric with busy_peers'
            // fail-closed running_nonempty (design.md §3.11). A
            // running/ entry with no tracked runpid is either a
            // ghost_entry quarantine failure (a dir nobody can kill;
            // gating poweroff on it would pin the box, and the bill,
            // forever on unrecoverable litter) or an operator's
            // manual mv. Firing poweroff past a ghost is the
            // recoverable direction: the next daemon's
            // requeue_orphans puts it back in pending/. A peer
            // daemon cannot make that distinction from outside, so
            // its gate stays fail-closed on the listing.
            if d.root.join("shutdown_on_idle").exists() && d.runpids.is_empty() && !shutdown::pending_nonempty(&d.root) {
                maybe_shutdown(&d.root, true, false, Some(&d.got_signal));
            } else if d.root.join("exit_on_idle").exists() && d.runpids.is_empty() && !shutdown::pending_nonempty(&d.root) {
                // Residency preference (§3.11a): the queue is drained
                // and the owner asked for no resident daemon — leave
                // the box clean. Not multi-tenant-gated: this daemon
                // only serves its own queue, so a busy sibling daemon
                // is no reason to stay, and `status`/dispatch
                // auto-heal revives it on the next submit.
                dlog(&d.root, "daemon_exit_idle", Map::new());
                let _ = std::fs::remove_file(d.root.join("daemon.pid"));
                shutdown::deregister_qroot(&d.root);
                return Ok(());
            }
        }
        std::thread::sleep(poll);
    }
}
