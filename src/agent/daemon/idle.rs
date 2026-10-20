//! Armed `shutdown_on_idle` countdown and exec (design.md §3.11).
//! Flag-state CRUD lives in `crate::agent::shutdown`; this module is
//! only the daemon-side orchestration around it.

use serde_json::{json, Map};
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
/// `abort_at`: signal counter — a signal arriving during the countdown
/// ends it early so the daemon returns to the poll loop and lets the
/// signal path run promptly (a second TERM during signal-path honoring
/// still kills the daemon; a TERM during an idle countdown re-enters
/// honoring through §3.5).
pub(crate) fn maybe_shutdown(root: &Path, running_empty: bool, stop_consumed: bool, abort_at: Option<&Arc<AtomicUsize>>) {
    let flag = root.join("shutdown_on_idle");
    // running/ is never re-checked during the countdown — the
    // entry-time bool is the only running check, and no dispatch
    // runs during the countdown so runpids cannot grow. A nonempty
    // running/ at entry therefore cannot drain inside the countdown
    // either (the signal path already TERM+KILLed and reaped what it
    // could): sleeping the whole grace just to reach the final
    // recheck's `running_empty` gate buys nothing. Bail now instead
    // (§3.5/§3.11).
    if !running_empty {
        dlog(root, "idle_shutdown_aborted", ev(json!({"why": "tasks survived signal path"})));
        return;
    }
    let grace = shutdown::grace(root);
    let base = abort_at.map(|c| c.load(Ordering::SeqCst));
    // No eprintln twin of this event: `shunt daemon`'s stderr IS
    // daemon.log (§3.1 strict JSONL), and `shunt shutdown on`
    // already tells the arming user on its own stdout.
    dlog(root, "idle_shutdown_armed", ev(json!({"grace_s": grace})));
    for i in (1..=grace).rev() {
        std::thread::sleep(Duration::from_secs(1));
        if let (Some(count), Some(base)) = (abort_at, base) {
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
    // final re-check immediately before firing
    let peers = shutdown::busy_peers(root);
    if flag.exists() && !shutdown::pending_nonempty(root) && running_empty && peers.is_empty() {
        let cmd = shutdown::resolve_cmd(root);
        dlog(root, "idle_shutdown_exec", ev(json!({"cmd": cmd})));
        // spawn, not exec: a failed launch must return here with the
        // pidfile and registry entry still intact — tearing down
        // before a failed exec left the daemon running headless,
        // invisible to alive()/busy_peers (design.md §3.11). The
        // spawned `sh -c` survives exit(0); on a spawn failure the
        // poll loop simply re-arms the countdown next cycle.
        match Command::new("sh").arg("-c").arg(&cmd).spawn() {
            Ok(_) => {
                let _ = std::fs::remove_file(root.join("daemon.pid"));
                shutdown::deregister_qroot(root);
                std::process::exit(0);
            }
            Err(e) => {
                dlog(root, "idle_shutdown_exec_failed", ev(json!({"error": e.to_string()})));
            }
        }
    } else {
        dlog(root, "idle_shutdown_aborted", Map::new());
    }
}

#[cfg(test)]
mod tests {
    /// running/ nonempty at entry cannot drain inside the countdown
    /// (§3.5: the signal path already TERM+KILLed; nothing reaps
    /// during honoring). maybe_shutdown must bail immediately
    /// instead of sleeping the full grace to reach the same verdict.
    #[test]
    fn nonempty_running_skips_countdown() {
        let root = std::env::temp_dir().join(format!("shunt-idle-test-{}", std::process::id()));
        std::fs::create_dir_all(root.join("pending")).unwrap();
        std::fs::write(root.join("shutdown_on_idle"), b"").unwrap();
        let t0 = std::time::Instant::now();
        super::maybe_shutdown(&root, false, false, None);
        assert!(t0.elapsed() < std::time::Duration::from_secs(5), "countdown slept despite live running tasks");
        let log = std::fs::read_to_string(root.join("daemon.log")).unwrap();
        assert!(log.contains("idle_shutdown_aborted"), "early bail must be logged");
        assert!(root.join("shutdown_on_idle").exists(), "flag stays armed");
        let _ = std::fs::remove_dir_all(&root);
    }
}
