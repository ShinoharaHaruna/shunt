//! Filesystem queue operations: track dirs, `.seq` counter under
//! flock, pending listing, enqueue install (design.md §3.1–3.3).

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

mod admission;
mod clean;
mod enqueue;
mod entries;
mod seq;
#[cfg(test)]
mod tests;

pub use clean::{clean, CleanReport, CleanSets};
pub(crate) use enqueue::invalidate_dedup_index;
pub use enqueue::{enqueue, enqueue_with_hold, EnqueueOutcome};
pub use entries::{archive_oldest, claim, digits_only, held_reason, hold_pending, is_dir_entry, list_entries, match_pending, match_running, pending_list, piece_file, recent_exits, release_pending, remove_pending, running_list, task_file};
pub use seq::{next_seq, seq_lock};

/// Sequence counter starts at 99 so the first task is `0100`;
/// `0000`–`0099` is the manual priority band (design.md §3.2).
pub const SEQ_INIT: u32 = 99;
pub const PRIORITY_BAND_MAX: u32 = 99;

/// Last usable sequence: names are fixed at 4 digits (`{n:04}`) and
/// claim order is byte sort — a 5-digit `10000_x` would sort ahead
/// of `9999_*` and break FIFO (design.md §3.2).
pub const SEQ_MAX: u32 = 9999;

/// Daemon identity for cmdline matching (design.md §3.4): the
/// subcommand string matched as a whole NUL-separated argv element
/// alongside argv[0]'s basename, never a substring — a substring
/// match misses renamed binaries (`shuntd`) and false-matches
/// unrelated ones (`foo-shunt daemon`).
pub const DAEMON_IDENTITY: &str = "daemon";

/// Built-in pool size when neither the file nor env supplies one.
pub const DEFAULT_WORKERS: u32 = 3;

/// Default idle-shutdown grace (design.md §3.11).
pub const DEFAULT_GRACE_S: u64 = 60;

/// Terminal-track cap: `done/`/`failed/` keep only the newest entries
/// on the live listing; older ones move to `<track>/.archive/`
/// (design.md §3.9). `SHUNT_TRACK_KEEP` overrides (tests).
pub fn track_keep() -> usize {
    std::env::var("SHUNT_TRACK_KEEP").ok().and_then(|s| s.parse::<usize>().ok()).unwrap_or(500)
}

/// `daemon.log` rotation threshold (design.md §3.1): past this size
/// the current log is renamed to `daemon.log.1` and a fresh file
/// starts. `SHUNT_DAEMON_LOG_MAX` overrides (tests).
pub fn daemon_log_max() -> u64 {
    std::env::var("SHUNT_DAEMON_LOG_MAX").ok().and_then(|s| s.parse::<u64>().ok()).unwrap_or(1 << 20)
}

/// Resolve the queue root: `--root` flag > `$SHUNT_ROOT` env.
pub fn resolve_root(flag: Option<&str>) -> Result<PathBuf> {
    if let Some(r) = flag {
        return Ok(PathBuf::from(r));
    }
    if let Ok(r) = std::env::var("SHUNT_ROOT") {
        return Ok(PathBuf::from(r));
    }
    anyhow::bail!("queue root unknown: pass --root or set SHUNT_ROOT")
}

/// Create the four track dirs and seed `workers` / `.seq`.
pub fn init(root: &Path) -> Result<()> {
    for d in ["pending", "running", "done", "failed"] {
        std::fs::create_dir_all(root.join(d)).with_context(|| format!("cannot create {}", root.join(d).display()))?;
    }
    let workers = root.join("workers");
    if !workers.exists() {
        std::fs::write(&workers, format!("{DEFAULT_WORKERS}\n"))?;
    }
    let seq = root.join(".seq");
    if !seq.exists() {
        std::fs::write(&seq, format!("{SEQ_INIT}\n"))?;
    }
    Ok(())
}
