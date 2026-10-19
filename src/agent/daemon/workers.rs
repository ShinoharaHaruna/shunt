//! Pool-size resolution (design.md §3.8, §3.14).

use std::path::Path;

use crate::agent::queue;

/// Desired pool size: `workers` file > `QUEUE_WORKERS` env >
/// `last_known` > default; digits extracted, clamped ≥ 1
/// (design.md §3.8).
pub(crate) fn desired_workers_in(root: &Path, last: Option<u32>) -> u32 {
    let w = if let Ok(s) = std::fs::read_to_string(root.join("workers")) {
        queue::digits_only(&s)
    } else if let Ok(e) = std::env::var("QUEUE_WORKERS") {
        e.trim().parse::<u32>().ok()
    } else {
        Some(queue::DEFAULT_WORKERS)
    };

    match w {
        Some(v) => v.max(1),
        None => last.unwrap_or(queue::DEFAULT_WORKERS).max(1),
    }
}

/// Resolved pool size + source for `status` / `workers get`
/// (never a hardcoded literal — bash defect fix, design.md §3.14).
pub fn resolve_workers(root: &Path) -> crate::proto::wire::WorkersState {
    if let Ok(s) = std::fs::read_to_string(root.join("workers")) {
        if let Some(v) = queue::digits_only(&s) {
            return crate::proto::wire::WorkersState { value: v.max(1), source: "file".into() };
        }
    }
    if let Ok(e) = std::env::var("QUEUE_WORKERS") {
        if let Ok(v) = e.trim().parse::<u32>() {
            return crate::proto::wire::WorkersState { value: v.max(1), source: "env".into() };
        }
    }
    crate::proto::wire::WorkersState { value: queue::DEFAULT_WORKERS, source: "default".into() }
}
