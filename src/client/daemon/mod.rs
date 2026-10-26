//! Daemon loop (design.md §3.5–3.10): reap → STOP drain or dispatch →
//! idle-shutdown check, on a 2 s poll (`POLL` env overridable).
//!
//! Module map:
//! - `core` — the poll loop (`run`), `Daemon`/`RunHandle` state.
//! - `dispatch` — claim → spawn → runpid tracking.
//! - `timeouts` — wall-clock budget enforcement (TERM → KILL).
//! - `lifecycle` — `alive`/`start`/`stop` (pidfile + /proc discipline).
//! - `workers` — pool-size resolution (`workers` file / env / default).
//! - `reap` — orphan requeue on startup and leader-exit finalize.
//! - `idle` — armed `shutdown_on_idle` grace countdown and exec.
//! - `exit` — the TERM/INT/HUP drain-and-exit path.

use serde_json::{json, Map};
use std::io::Write;
use std::path::Path;
use std::time::Duration;

mod core;
mod dispatch;
mod exit;
mod idle;
mod lifecycle;
mod reap;
#[cfg(test)]
mod tests;
mod timeouts;
mod workers;

pub use core::run;
pub use lifecycle::{alive, check_alive, restart, start, stop, Restart, StartOutcome};
pub use workers::resolve_workers;

/// Remove the pidfile trio on every exit/eviction path (§3.4):
/// `daemon.pid` is the liveness source, `daemon.ver`/`daemon.bin`
/// the §3.5a handoff gate — a stopped daemon must not leave a
/// capability marker that `restart` would gate on before liveness.
pub(crate) fn drop_daemon_files(root: &Path) {
    for f in ["daemon.pid", "daemon.ver", "daemon.bin"] {
        let _ = std::fs::remove_file(root.join(f));
    }
}

/// Event fields helper: json! object -> Map for dlog.
pub(crate) fn ev(v: serde_json::Value) -> Map<String, serde_json::Value> {
    v.as_object().cloned().unwrap_or_default()
}

/// Append one JSONL event to daemon.log: `{ts, event, ...}`.
/// The log is unbounded without rotation: past `daemon_log_max()` it
/// is renamed to `daemon.log.1` (one generation kept) and a fresh
/// file starts (design.md §3.1).
pub(crate) fn dlog(root: &Path, event: &str, extra: Map<String, serde_json::Value>) {
    let mut obj = Map::new();
    obj.insert("ts".into(), json!(crate::proto::time::rfc3339_offset_now()));
    obj.insert("event".into(), json!(event));
    obj.extend(extra);
    let path = root.join("daemon.log");
    if let Ok(m) = path.metadata() {
        if m.len() > crate::client::queue::daemon_log_max() {
            let _ = std::fs::rename(&path, root.join("daemon.log.1"));
        }
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{}", serde_json::Value::Object(obj));
    }
}

/// Poll interval: `POLL` env (fractional seconds ok) or 2 s.
pub(crate) fn poll_interval() -> Duration {
    std::env::var("POLL").ok().and_then(|s| s.parse::<f64>().ok()).filter(|p| *p > 0.0).map(Duration::from_secs_f64).unwrap_or_else(|| Duration::from_secs(2))
}
