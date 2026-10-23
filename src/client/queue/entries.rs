//! Track listings and entry movement: claim, cancel, archive, and
//! the dir-vs-legacy-file entry shapes (design.md §3.1, §3.9).

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Archive the oldest entries of a terminal track beyond `keep`:
/// they move to `<track>/.archive/` — dot-prefixed, so `list_entries`
/// (which skips `.` names) never sees them, but the data survives on
/// disk. Name sort is sequence order, so the oldest entries leave
/// first. Returns how many entries moved.
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
    for name in names.iter().take(names.len() - keep) {
        if std::fs::rename(root.join(track).join(name), archive.join(name)).is_ok() {
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

/// Atomic claim: pending/<name> -> running/<name>. Ok(false) when the
/// entry vanished under us (a concurrent cancel/claim) — the rename
/// itself is the recheck, never a prior existence test. Err means the
/// entry still exists but cannot be claimed (name collision in
/// running/, permissions): the caller quarantines it to failed/
/// rather than spinning on the same unclaimable head every poll.
pub fn claim(root: &Path, name: &str) -> Result<bool> {
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
        // the startup sweep in requeue_orphans.
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
