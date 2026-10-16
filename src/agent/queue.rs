//! Filesystem queue operations: track dirs, `.seq` counter under
//! flock, pending listing, enqueue install (design.md §3.1–3.3).

use anyhow::{Context, Result};
use std::path::PathBuf;

/// Sequence counter starts at 99 so the first task is `0100`;
/// `0000`–`0099` is the manual priority band (design.md §3.2).
pub const SEQ_INIT: u32 = 99;
pub const PRIORITY_BAND_MAX: u32 = 99;

/// Liveness identity string for cmdline matching (design.md §3.4).
/// Single-sourced here; the bash version duplicated it in three
/// scripts, which was a logged defect.
pub const DAEMON_IDENTITY: &str = "shunt daemon";

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

/// Create the four track dirs and friends. Stub.
pub fn init(_root: &std::path::Path) -> Result<()> {
    anyhow::bail!("queue::init: not yet implemented")
}

/// Next sequence number under flock on `.seq.lock` (30 s wait,
/// matching `flock -w 30`). Stub.
pub fn next_seq(_root: &std::path::Path) -> Result<u32> {
    anyhow::bail!("queue::next_seq: not yet implemented")
}

/// Pending filenames in `LC_ALL=C` byte sort order = FIFO claim order.
/// Real: a directory read plus sort is cheap and pure.
pub fn pending_list(root: &std::path::Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    let dir = root.join("pending");
    match std::fs::read_dir(&dir) {
        Ok(rd) => {
            for ent in rd {
                let name = ent?.file_name().to_string_lossy().into_owned();
                if name.ends_with(".sh") {
                    names.push(name);
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", dir.display())),
    }
    names.sort();
    Ok(names)
}

/// Install a task body into pending/ atomically (write temp inside
/// pending/, then mv). Stub.
pub fn enqueue(_root: &std::path::Path, _slug: &str, _body: &str) -> Result<String> {
    anyhow::bail!("queue::enqueue: not yet implemented")
}
