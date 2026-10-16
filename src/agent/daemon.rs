//! Daemon loop (design.md §3.5–3.10): reap → STOP drain or dispatch →
//! idle-shutdown check, on a 2 s poll (`POLL` env overridable).
//! All stubs; semantics live in the spec until implemented.

use anyhow::Result;
use std::path::Path;

/// Daemon liveness: pidfile + `kill -0` + `/proc/<pid>/cmdline`
/// contains `queue::DAEMON_IDENTITY`. Never pgrep (design.md §3.4).
/// Stub.
pub fn alive(_root: &Path) -> Result<Option<u32>> {
    anyhow::bail!("daemon::alive: not yet implemented")
}

/// Run the queue loop in the foreground. Stub.
pub fn run(_root: &Path) -> Result<()> {
    anyhow::bail!("daemon::run: not yet implemented")
}

/// Self-daemonize: setsid, redirect to daemon.log, write pidfile.
/// Stub.
pub fn start(_root: &Path) -> Result<u32> {
    anyhow::bail!("daemon::start: not yet implemented")
}

/// Signal the daemon to stop (TERM). Stub.
pub fn stop(_root: &Path) -> Result<()> {
    anyhow::bail!("daemon::stop: not yet implemented")
}

/// Orphan requeue: running/*.sh from a dead daemon go to
/// `pending/0000_requeued_*` (design.md §3.6). Stub.
pub fn requeue_orphans(_root: &Path) -> Result<()> {
    anyhow::bail!("daemon::requeue_orphans: not yet implemented")
}
