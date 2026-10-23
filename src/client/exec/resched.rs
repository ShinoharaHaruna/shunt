//! Retry reschedule: rc 75 / OOM-heuristic requeue back into
//! pending/ under the same name (design.md §3.9).

use anyhow::{Context, Result};
use std::path::Path;

use super::directives::TaskDirectives;
use crate::client::queue;

/// Default wait before a rescheduled task may be claimed again when
/// the task doesn't declare `# shunt:retry_delay_s=` (design.md
/// §3.9). Long enough that a transient co-tenant burst can clear;
/// short enough that a paid box doesn't idle for a wrong guess.
pub const DEFAULT_RETRY_DELAY_S: u64 = 60;

/// What `maybe_reschedule` decided — `Queued` reports the requeue
/// details for the daemon's `task_rescheduled` event.
pub enum Resched {
    /// Not eligible: no retry budget declared, budget exhausted,
    /// the rc isn't reschedule-shaped, or a same-named pending
    /// entry already holds the target.
    No,
    /// Moved back to `pending/` under its original name.
    Queued { attempt: u32, via: &'static str, next_at: u64 },
}

/// Bounded OOM-signature scan over the tail of a running task's
/// log — the heuristic half of reschedule eligibility (design.md
/// §3.9): only tasks that declared `# shunt:retry=` ever reach it,
/// so a misread costs at most a bounded rerun, and a task that
/// reports rc 75 itself never needs the scan at all.
fn log_says_oom(root: &Path, base: &str) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let log = queue::piece_file(root, "running", base, "log");
    let Ok(mut f) = std::fs::File::open(&log) else {
        return false;
    };
    // OOM tracebacks sit at the end of the log; 16 KiB covers the
    // longest plausible traceback block.
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let _ = f.seek(SeekFrom::Start(len.saturating_sub(16 * 1024)));
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return false;
    }
    let hay = String::from_utf8_lossy(&buf).to_lowercase();
    ["out of memory", "outofmemoryerror", "cuda_error_out_of_memory", "alloc_failed", "cannot allocate memory"].iter().any(|p| hay.contains(p))
}

/// Reschedule a failed task instead of finalizing it (design.md
/// §3.9): the task opts in with `# shunt:retry=N`, reports
/// "resource was busy" via rc 75 (EX_TEMPFAIL), or — because most
/// real OOM exits are plain rc 1 — trips the log heuristic. The
/// entry keeps its `NNNN_` name: the number stays leased to this
/// task, `task show`'s lazy queue_name match keeps resolving, and
/// it rejoins pending/ at its sorted position. The failed
/// attempt's log is swept to `failed/<base>.attempt<K>.log` — spawn
/// truncates `log` each run, so keeping it inside would destroy
/// the evidence. `attempts`/`retry_after` sidecars inside the
/// entry carry the consumed budget and the wait deadline.
/// Returns `Resched::No` when the task should just finalize
/// normally; `Err` means the requeue itself broke (the caller
/// finalizes anyway and logs the failure loudly).
pub fn maybe_reschedule(root: &Path, base: &str, rc: i32, td: &TaskDirectives) -> Result<Resched> {
    let Some(max_retry) = td.retry.filter(|&n| n > 0) else {
        return Ok(Resched::No);
    };
    let entry = root.join("running").join(base);
    if !entry.is_dir() {
        return Ok(Resched::No); // legacy flat entry: no sidecar home
    }
    let attempts: u32 = std::fs::read_to_string(entry.join("attempts")).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    if attempts >= max_retry {
        return Ok(Resched::No); // budget exhausted -> failed/
    }
    let via = if rc == 75 {
        "rc75"
    } else if log_says_oom(root, base) {
        "oom_heuristic"
    } else {
        return Ok(Resched::No);
    };
    // A backward move can evade enqueue's pending -> running scan.
    // Serialize the collision check and mutation with that scan; this
    // local guard drops on every return, before finalize or another spawn.
    let _lock = queue::seq_lock(root)?;
    let pending = root.join("pending");
    if pending.join(base).exists() || pending.join(format!("{base}.sh")).exists() {
        // Same-numbered pending entry already exists — requeueing
        // would collide. Finalize normally; the caller logs the
        // finish line, and the anomaly is visible in failed/.
        return Ok(Resched::No);
    }
    // Sidecars go in BEFORE the move so a failed write bails while
    // the entry still sits in running/ — the caller then finalizes
    // it normally rather than losing the budget bookkeeping.
    let attempt = attempts + 1;
    std::fs::write(entry.join("attempts"), format!("{attempt}\n"))?;
    let next_at = crate::proto::time::unix_now() + td.retry_delay_s.unwrap_or(DEFAULT_RETRY_DELAY_S);
    std::fs::write(entry.join("retry_after"), format!("{next_at}\n"))?;
    let _ = std::fs::remove_file(entry.join("pid"));
    std::fs::create_dir_all(root.join("failed"))?;
    if entry.join("log").exists() {
        let _ = std::fs::rename(entry.join("log"), root.join("failed").join(format!("{base}.attempt{attempt}.log")));
    }
    std::fs::rename(&entry, pending.join(base)).with_context(|| format!("cannot requeue {base} into pending/"))?;
    Ok(Resched::Queued { attempt, via, next_at })
}
