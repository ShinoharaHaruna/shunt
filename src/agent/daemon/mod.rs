//! Daemon loop (design.md §3.5–3.10): reap → STOP drain or dispatch →
//! idle-shutdown check, on a 2 s poll (`POLL` env overridable).
//!
//! Module map:
//! - `core` — the poll loop (`run`), `Daemon` state, dispatch claim
//!   loop, reap/finalize, timeout enforcement, signal exit.
//! - `lifecycle` — `alive`/`start`/`stop` (pidfile + /proc discipline).
//! - `workers` — pool-size resolution (`workers` file / env / default).
//! - `reap` — orphan requeue on daemon startup.
//! - `idle` — armed `shutdown_on_idle` grace countdown and exec.

use serde_json::{json, Map};
use std::io::Write;
use std::path::Path;
use std::time::Duration;

mod core;
mod idle;
mod lifecycle;
mod reap;
mod workers;

pub use core::run;
pub use lifecycle::{alive, start, stop};
pub use workers::resolve_workers;

/// Event fields helper: json! object -> Map for dlog.
pub(crate) fn ev(v: serde_json::Value) -> Map<String, serde_json::Value> {
    v.as_object().cloned().unwrap_or_default()
}

/// Append one JSONL event to daemon.log: `{ts, event, ...}`.
pub(crate) fn dlog(root: &Path, event: &str, extra: Map<String, serde_json::Value>) {
    let mut obj = Map::new();
    obj.insert("ts".into(), json!(crate::agent::queue::iso8601_now()));
    obj.insert("event".into(), json!(event));
    obj.extend(extra);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(root.join("daemon.log")) {
        let _ = writeln!(f, "{}", serde_json::Value::Object(obj));
    }
}

/// Poll interval: `POLL` env (fractional seconds ok) or 2 s.
pub(crate) fn poll_interval() -> Duration {
    std::env::var("POLL").ok().and_then(|s| s.parse::<f64>().ok()).filter(|p| *p > 0.0).map(Duration::from_secs_f64).unwrap_or_else(|| Duration::from_secs(2))
}
