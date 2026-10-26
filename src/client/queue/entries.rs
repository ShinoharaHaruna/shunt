//! Track listings and entry movement: claim, cancel, archive, and
//! the dir-vs-legacy-file entry shapes (design.md §3.1, §3.9).

use super::admission::admission_lock;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Archive the oldest entries of a terminal track beyond `keep`:
/// they move to `<track>/.archive/` — dot-prefixed, so `list_entries`
/// (which skips `.` names) never sees them, but the data survives on
/// disk. Name sort is sequence order, so the oldest entries leave
/// first. The underscore prefix releases their NNNN slots; old-format
/// NNNN archives remain inert and cannot collide with these names.
/// Returns how many entries moved.
pub fn archive_oldest(root: &Path, track: &str, keep: usize) -> usize {
    let Ok(names) = list_entries(root, track) else {
        return 0;
    };
    if names.len() <= keep {
        return 0;
    }
    let archive = root.join(track).join(".archive");
    if std::fs::create_dir_all(&archive).is_err() {
        return 0;
    }
    let mut moved = 0;
    let millis = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis();
    for name in names.iter().take(names.len() - keep) {
        let archived_name = format!("_{millis}_{name}");
        let target = archive.join(archived_name);
        // Unix rename can replace an existing file; preserve same-ms
        // collisions in the terminal track for a later finalize retry.
        if target.exists() {
            continue;
        }
        if std::fs::rename(root.join(track).join(name), target).is_ok() {
            moved += 1;
        }
    }
    moved
}

/// Entry base names in a track, `LC_ALL=C` byte sort. A queue entry
/// is a directory `NNNN_slug/` (current layout: `task.sh` plus
/// sidecars `log`/`pid`/`exit` inside) or a legacy flat file
/// `NNNN_slug.sh` (sidecars `.sh.log`/`.sh.exit`/`.sh.pid`). Both
/// forms read identically here — old queues need no migration.
pub fn list_entries(root: &Path, track: &str) -> Result<Vec<String>> {
    let mut names = Vec::new();
    let dir = root.join(track);
    match std::fs::read_dir(&dir) {
        Ok(rd) => {
            for ent in rd.flatten() {
                let name = ent.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') {
                    continue;
                }
                if let Ok(ft) = ent.file_type() {
                    if ft.is_dir() {
                        names.push(name);
                    } else if let Some(stem) = name.strip_suffix(".sh") {
                        names.push(stem.to_string());
                    }
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", dir.display())),
    }
    names.sort();
    Ok(names)
}

/// Pending entry names in claim order.
pub fn pending_list(root: &Path) -> Result<Vec<String>> {
    list_entries(root, "pending")
}

/// Running entry names, sorted.
pub fn running_list(root: &Path) -> Result<Vec<String>> {
    list_entries(root, "running")
}

/// True when the entry is the current directory layout.
pub fn is_dir_entry(root: &Path, track: &str, name: &str) -> bool {
    root.join(track).join(name).is_dir()
}

/// The task script inside an entry (`name/task.sh` for dirs,
/// `name.sh` for legacy flat files).
pub fn task_file(root: &Path, track: &str, name: &str) -> PathBuf {
    let d = root.join(track).join(name);
    if d.is_dir() {
        d.join("task.sh")
    } else {
        root.join(track).join(format!("{name}.sh"))
    }
}

/// A sidecar file: `name/<piece>` inside a dir entry,
/// `name.sh.<piece>` for a legacy flat entry (piece: log, pid, exit).
pub fn piece_file(root: &Path, track: &str, name: &str, piece: &str) -> PathBuf {
    let d = root.join(track).join(name);
    if d.is_dir() {
        d.join(piece)
    } else {
        root.join(track).join(format!("{name}.sh.{piece}"))
    }
}

/// Hold reason for a current directory entry. `Some("")` is a held
/// entry without a reason; flat legacy entries cannot carry this gate.
pub fn held_reason(root: &Path, track: &str, name: &str) -> Result<Option<String>> {
    if !is_dir_entry(root, track, name) {
        return Ok(None);
    }
    let path = root.join(track).join(name).join("held");
    match std::fs::read_to_string(&path) {
        Ok(reason) => Ok(Some(reason)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

fn require_pending_dir(root: &Path, name: &str, operation: &str) -> Result<PathBuf> {
    let pending = root.join("pending");
    let entry = pending.join(name);
    match std::fs::symlink_metadata(&entry) {
        Ok(meta) if meta.file_type().is_dir() => return Ok(entry),
        Ok(_) => anyhow::bail!("cannot {operation} {name}: pending/{name} is not an entry directory; legacy flat entries have no sidecar home, so enqueue it again with the current shunt binary"),
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e).with_context(|| format!("cannot inspect {}", entry.display())),
        Err(_) => {}
    }
    if pending.join(format!("{name}.sh")).is_file() {
        anyhow::bail!("cannot {operation} legacy flat entry {name}: readers support flat entries, but held sidecars require pending/{name}/; enqueue it again with the current shunt binary")
    }
    if root.join("running").join(name).exists() {
        anyhow::bail!("cannot {operation} {name}: it is already running; running tasks cannot be re-parked or released")
    }
    anyhow::bail!("no pending task named {name}; it may have been claimed or removed, so re-run `shunt status` and retry with the pending entry name")
}

/// Write a pending entry's admission marker. The dedicated lock
/// makes an acknowledged hold win over a concurrent claim.
pub fn hold_pending(root: &Path, name: &str, reason: &str) -> Result<()> {
    let _lock = admission_lock(root)?;
    let entry = require_pending_dir(root, name, "hold")?;
    let path = entry.join("held");
    std::fs::write(&path, reason).with_context(|| format!("cannot write {}", path.display()))
}

/// Clear a pending entry's one-shot admission marker. Releasing an
/// already-claimable entry is harmless and idempotent.
pub fn release_pending(root: &Path, name: &str) -> Result<()> {
    let _lock = admission_lock(root)?;
    let entry = require_pending_dir(root, name, "release")?;
    let path = entry.join("held");
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("cannot remove {}", path.display())),
    }
}

/// Remove a pending entry whatever its shape (dir or legacy file).
pub fn remove_pending(root: &Path, name: &str) -> Result<()> {
    let d = root.join("pending").join(name);
    if d.is_dir() {
        std::fs::remove_dir_all(&d).with_context(|| format!("cannot remove {}", d.display()))?;
    } else {
        std::fs::remove_file(root.join("pending").join(format!("{name}.sh"))).with_context(|| format!("cannot remove {name}.sh"))?;
    }
    Ok(())
}

/// Atomic claim: pending/<name> -> running/<name>. Hold/release use
/// the same lock, so a successful hold cannot race past this final gate.
/// Ok(false) means the entry vanished, is held, or is waiting out
/// retry_after; Err means a present entry cannot be claimed.
pub fn claim(root: &Path, name: &str) -> Result<bool> {
    let _lock = admission_lock(root)?;
    let pending = root.join("pending");
    let entry = pending.join(name);
    if entry.is_dir() {
        let held = entry.join("held");
        match std::fs::symlink_metadata(&held) {
            Ok(_) => return Ok(false),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("cannot inspect {}", held.display())),
        }
    }
    let retry_after = piece_file(root, "pending", name, "retry_after");
    if retry_after.exists() {
        let due = std::fs::read_to_string(&retry_after).ok().and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(0);
        if due > crate::proto::time::unix_now() {
            return Ok(false);
        }
        let _ = std::fs::remove_file(retry_after);
    }
    claim_locked(root, name)
}

/// Caller holds `.admission.lock`; held/retry_after checks and the
/// move are one admission decision.
fn claim_locked(root: &Path, name: &str) -> Result<bool> {
    let pending = root.join("pending");
    let dst = root.join("running").join(name);
    if !pending.join(name).is_dir() {
        // Legacy flat file: normalize into the dir layout INSIDE
        // pending/ first, so `running/<name>` only ever appears via
        // one atomic rename of a COMPLETE entry. The old path
        // mkdir'd running/<name> before the rename — a kill -9 in
        // that window stranded an empty pid-less ghost dir that
        // counts in running_list and keeps busy_peers busy forever
        // (blocking a sibling's idle-shutdown). The staging dir is
        // dot-prefixed, so listings never see it; a stage stranded
        // by kill -9 is recovered to a full pending/<name> dir by
        // the startup sweep in adopt_orphans.
        let src = pending.join(format!("{name}.sh"));
        if !src.exists() {
            return Ok(false);
        }
        if pending.join(name).exists() {
            // A stray non-.sh file holds the namespace; unclaimable.
            anyhow::bail!("cannot claim {name}: pending/{name} already exists");
        }
        let stage = pending.join(format!(".tmp.claim.{}.{name}", std::process::id()));
        std::fs::create_dir_all(&stage).with_context(|| format!("cannot claim {name}"))?;
        if let Err(e) = std::fs::rename(&src, stage.join("task.sh")) {
            // A failed .sh move must not strand the staging dir; the
            // entry is still flat-listed, so lost-race is Ok(false).
            let _ = std::fs::remove_dir_all(&stage);
            return if e.kind() == std::io::ErrorKind::NotFound { Ok(false) } else { Err(e).with_context(|| format!("cannot claim {name}")) };
        }
        if let Err(e) = std::fs::rename(&stage, pending.join(name)) {
            // Restore the flat file so the entry stays listed and
            // claimable rather than hiding in an invisible stage.
            let _ = std::fs::rename(stage.join("task.sh"), &src);
            let _ = std::fs::remove_dir_all(&stage);
            return Err(e).with_context(|| format!("cannot claim {name}"));
        }
        // Fall through to the atomic dir claim.
    }
    if dst.exists() {
        anyhow::bail!("cannot claim {name}: running/{name} already exists");
    }
    match std::fs::rename(pending.join(name), &dst) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("cannot claim {name}")),
    }
}

/// Pending entries whose name contains `pat`, sorted.
pub fn match_pending(root: &Path, pat: &str) -> Result<Vec<String>> {
    Ok(pending_list(root)?.into_iter().filter(|n| n.contains(pat)).collect())
}

/// Running entries that have a pid file and whose name contains
/// `pat`, sorted.
pub fn match_running(root: &Path, pat: &str) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for name in running_list(root)? {
        if !name.contains(pat) {
            continue;
        }
        if piece_file(root, "running", &name, "pid").exists() {
            names.push(name);
        }
    }
    Ok(names)
}

/// Recent finished tasks in a track: `(entry name, rc)` newest first.
pub fn recent_exits(root: &Path, track: &str, n: usize) -> Vec<(String, i32)> {
    let mut rows: Vec<(SystemTime, String)> = Vec::new();
    for name in list_entries(root, track).unwrap_or_default() {
        let ef = piece_file(root, track, &name, "exit");
        if !ef.exists() {
            continue;
        }
        let mtime = ef.metadata().and_then(|m| m.modified()).unwrap_or(UNIX_EPOCH);
        rows.push((mtime, name));
    }
    rows.sort_by_key(|r| std::cmp::Reverse(r.0));
    rows.truncate(n);
    rows.into_iter()
        .map(|(_, f)| {
            let rc = std::fs::read_to_string(piece_file(root, track, &f, "exit")).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(-1);
            (f, rc)
        })
        .collect()
}

/// Extract digits from a string (`tr -cd '0-9'` semantics); `None`
/// when nothing or the value overflows.
pub fn digits_only(s: &str) -> Option<u32> {
    let d: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
    d.parse().ok()
}
