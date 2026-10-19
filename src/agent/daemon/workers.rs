//! Pool-size resolution (design.md §3.8, §3.14). One function serves
//! both the daemon poll loop and `status`/`workers get` so the two
//! can never disagree: `workers` file > `QUEUE_WORKERS` env > last
//! valid value > default. The daemon's "last valid" is in-memory;
//! one-shot callers read `.workers.last`, which the daemon refreshes
//! whenever the resolved value changes — so `status` reports what
//! the running daemon would actually use.

use std::path::Path;

use crate::agent::queue;

/// Last-resolved pool size persisted by the daemon (design.md §3.1).
fn last_file(root: &Path) -> std::path::PathBuf {
    root.join(".workers.last")
}

/// Daemon-side: remember the value it resolved so a one-shot
/// `status` sees the same last-known fallback.
pub(crate) fn persist_last(root: &Path, v: u32) {
    let _ = std::fs::write(last_file(root), format!("{v}\n"));
}

/// Read a persisted last-known value (None when absent or garbage —
/// a corrupt `.workers.last` degrades to default, same as a fresh
/// daemon).
pub fn read_last(root: &Path) -> Option<u32> {
    std::fs::read_to_string(last_file(root)).ok().and_then(|s| s.trim().parse::<u32>().ok()).filter(|&n| n > 0)
}

/// The single resolution path (design.md §3.8): a valid `workers`
/// file wins; else a valid `QUEUE_WORKERS`; else `last` (daemon's
/// memory, or `.workers.last` for one-shot callers); else default.
/// Digits are extracted from the file (`tr -cd '0-9'` semantics);
/// everything is clamped ≥ 1.
fn resolve(root: &Path, last: Option<u32>) -> crate::proto::wire::WorkersState {
    use crate::proto::wire::WorkersState;
    if let Ok(s) = std::fs::read_to_string(root.join("workers")) {
        if let Some(v) = queue::digits_only(&s) {
            return WorkersState { value: v.max(1), source: "file".into() };
        }
    }
    if let Ok(e) = std::env::var("QUEUE_WORKERS") {
        // Same `tr -cd '0-9'` semantics as the file path (§3.8).
        if let Some(v) = queue::digits_only(&e) {
            return WorkersState { value: v.max(1), source: "env".into() };
        }
    }
    if let Some(v) = last {
        return WorkersState { value: v.max(1), source: "last-known".into() };
    }
    WorkersState { value: queue::DEFAULT_WORKERS, source: "default".into() }
}

/// Desired pool size for the daemon loop; `last` is the daemon's
/// in-memory last-valid value.
pub(crate) fn desired_workers_in(root: &Path, last: Option<u32>) -> u32 {
    resolve(root, last).value
}

/// Resolved pool size + source for `status` / `workers get`
/// (never a hardcoded literal — bash defect fix, design.md §3.14).
/// Shares the daemon's `resolve`; `last` comes from `.workers.last`,
/// so a garbage `workers` file reports the value the daemon is
/// actually running ("last-known"), not a divergent "default".
pub fn resolve_workers(root: &Path) -> crate::proto::wire::WorkersState {
    resolve(root, read_last(root))
}
