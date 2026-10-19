//! Armed `shutdown_on_idle` countdown and exec (design.md §3.11).
//! Flag-state CRUD lives in `crate::agent::shutdown`; this module is
//! only the daemon-side orchestration around it.

use serde_json::{json, Map};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::{dlog, ev};
use crate::agent::shutdown;

/// Idle-shutdown countdown (design.md §3.11). Returns when cancelled;
/// on success it execs the shutdown command and never returns.
/// `stop_consumed`: caller already removed the STOP sentinel, so it
/// must not cancel this countdown (STOP drain honoring an armed flag).
/// `abort_at`: `(signal counter, base)` — a further signal past `base`
/// cancels the countdown, so a second TERM during signal-path honoring
/// still kills the daemon promptly.
pub(crate) fn maybe_shutdown(root: &Path, running_empty: bool, stop_consumed: bool, abort_at: Option<(&Arc<AtomicUsize>, usize)>) {
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
