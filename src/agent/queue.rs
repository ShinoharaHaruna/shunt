//! Filesystem queue operations: track dirs, `.seq` counter under
//! flock, pending listing, enqueue install (design.md §3.1–3.3).

use anyhow::{Context, Result};
use fs2::FileExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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

/// Read + increment `.seq` under an flock on `.seq.lock`, retrying
/// for up to 30 s (matching `flock -w 30`).
pub fn next_seq(root: &Path) -> Result<u32> {
    let lock_path = root.join(".seq.lock");
    let lock = std::fs::OpenOptions::new().create(true).append(true).open(&lock_path).with_context(|| format!("cannot open {}", lock_path.display()))?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match lock.try_lock_exclusive() {
            Ok(()) => break,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    anyhow::bail!("timed out waiting for .seq.lock");
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => return Err(e).context("flock .seq.lock failed"),
        }
    }
    let seq_path = root.join(".seq");
    let cur: u32 = std::fs::read_to_string(&seq_path).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(SEQ_INIT);
    // The new entry must sort BEHIND every queued one (claim order
    // is byte sort on the 4-digit prefix). Normally `cur + 1`
    // qualifies; when the counter wrapped at SEQ_MAX (or a
    // hand-placed number sits higher) continue just past the
    // high-water mark instead. The floor spans ALL tracks and
    // archives, not just pending/: a same-numbered entry in
    // running/ would fail the claim, one in done//failed/ would
    // fail the finalize rename — both unclaimable-until-quarantine.
    // True exhaustion — 9999 already occupied — errors out rather
    // than minting a 5-digit name that would leapfrog the queue.
    let hi = high_water(root)?;
    let next = if cur >= SEQ_MAX {
        // Counter wrapped the 4-digit ceiling: restart just past
        // the highest occupied number.
        hi + 1
    } else {
        // A hand-placed entry above `cur` still forces the
        // new task to sort behind it.
        (cur + 1).max(hi + 1)
    };
    if next > SEQ_MAX {
        anyhow::bail!("sequence space exhausted: a track already holds {SEQ_MAX}; let tasks drain or cancel some");
    }
    std::fs::write(&seq_path, format!("{next}\n"))?;
    lock.unlock()?;
    Ok(next)
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

/// Highest NNNN prefix occupied anywhere in the NNNN namespace:
/// all four live tracks plus each track's `.archive/` (priority
/// band 0000–0099 excluded). Wraparound renumbering must clear
/// every one of these or the minted name collides at claim or
/// finalize time (design.md §3.2).
fn high_water(root: &Path) -> Result<u32> {
    let mut hi = SEQ_INIT;
    let mut bump = |n: &str| {
        if let Some(v) = n.get(..4).and_then(|p| p.parse::<u32>().ok()).filter(|&v| v > PRIORITY_BAND_MAX) {
            hi = hi.max(v);
        }
    };
    for track in ["pending", "running", "done", "failed"] {
        for name in list_entries(root, track)? {
            bump(&name);
        }
        // Archived entries still hold their NNNN prefix; a wrapped
        // counter minting onto one collides in .archive/ itself.
        if let Ok(rd) = std::fs::read_dir(root.join(track).join(".archive")) {
            for ent in rd.flatten() {
                bump(&ent.file_name().to_string_lossy());
            }
        }
    }
    Ok(hi)
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
    if pending.join(name).is_dir() {
        if dst.exists() {
            anyhow::bail!("cannot claim {name}: running/{name} already exists");
        }
        return match std::fs::rename(pending.join(name), &dst) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e).with_context(|| format!("cannot claim {name}")),
        };
    }
    // Legacy flat file: normalize into the dir layout on claim.
    let src = pending.join(format!("{name}.sh"));
    if !src.exists() {
        return Ok(false);
    }
    if dst.exists() {
        anyhow::bail!("cannot claim {name}: running/{name} already exists");
    }
    std::fs::create_dir_all(&dst).with_context(|| format!("cannot claim {name}"))?;
    match std::fs::rename(&src, dst.join("task.sh")) {
        Ok(()) => Ok(true),
        Err(e) => {
            // The dst dir we just created must not outlive the lost
            // race: an empty pid-less `running/<name>/` ghost counts
            // in running_list and keeps busy_peers busy forever.
            // remove_dir fails harmlessly if it is somehow non-empty
            // — that would mean someone else owns it.
            let _ = std::fs::remove_dir(&dst);
            if e.kind() == std::io::ErrorKind::NotFound {
                Ok(false)
            } else {
                Err(e).with_context(|| format!("cannot claim {name}"))
            }
        }
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

/// Install a task body into pending/ as `NNNN_<slug>/task.sh`, mode
/// 0755, via write-temp-dir-then-mv inside pending/ (atomic same-fs
/// rename). Returns the entry name (`NNNN_<slug>`).
pub fn enqueue(root: &Path, slug: &str, body: &str) -> Result<String> {
    use std::os::unix::fs::PermissionsExt;
    let n = next_seq(root)?;
    let name = format!("{n:04}_{slug}");
    let pending = root.join("pending");
    std::fs::create_dir_all(&pending)?;
    let tmp = pending.join(format!(".tmp.{}.{}", std::process::id(), n));
    std::fs::create_dir_all(&tmp)?;
    let result = (|| -> Result<()> {
        let tf = tmp.join("task.sh");
        std::fs::write(&tf, body)?;
        std::fs::set_permissions(&tf, std::fs::Permissions::from_mode(0o755))?;
        std::fs::rename(&tmp, pending.join(&name))?;
        Ok(())
    })();
    // Any failure after the staging dir exists must not strand a
    // `.tmp.*` dir in pending/ (dot-prefixed, so listings skip it,
    // but it litters the track forever).
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&tmp);
    }
    result?;
    Ok(name)
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

/// RFC 3339 UTC timestamp (`date -Is` equivalent).
pub fn iso8601_now() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
    let days = secs.div_euclid(86400);
    let sod = secs.rem_euclid(86400);
    // civil-from-days (Howard Hinnant's algorithm)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}+00:00", sod / 3600, (sod % 3600) / 60, sod % 60)
}

/// Seconds since the unix epoch.
pub fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

/// Extract digits from a string (`tr -cd '0-9'` semantics); `None`
/// when nothing or the value overflows.
pub fn digits_only(s: &str) -> Option<u32> {
    let d: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
    d.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("shunt-qtest-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        init(&root).unwrap();
        root
    }

    /// Losing the flat-file claim race must not strand an empty
    /// `running/<name>/` ghost: it would count in running_list and
    /// keep busy_peers busy forever, blocking idle-shutdown.
    #[test]
    fn claim_lost_race_leaves_no_ghost_dir() {
        let root = tmp_root("ghost");
        std::fs::write(root.join("pending/0100_x.sh"), "echo hi\n").unwrap();
        // cancel wins the race before the rename lands
        std::fs::remove_file(root.join("pending/0100_x.sh")).unwrap();
        assert!(!claim(&root, "0100_x").unwrap());
        assert!(running_list(&root).unwrap().is_empty());
        assert!(!root.join("running/0100_x").exists());
        std::fs::remove_dir_all(&root).ok();
    }

    /// The wraparound floor spans all tracks + archives (§3.2): a
    /// number held only in done/ or .archive/ must not be reminted —
    /// the collision would fail a later claim/finalize rename.
    #[test]
    fn next_seq_wraparound_clears_all_tracks() {
        let root = tmp_root("hiwater");
        std::fs::create_dir_all(root.join("done/9500_old")).unwrap();
        std::fs::create_dir_all(root.join("failed/.archive/9700_ancient")).unwrap();
        std::fs::write(root.join(".seq"), format!("{SEQ_MAX}\n")).unwrap();
        let n = next_seq(&root).unwrap();
        assert_eq!(n, 9701);
        // true exhaustion: 9999 occupied in a terminal track
        std::fs::create_dir_all(root.join("done/9999_last")).unwrap();
        assert!(next_seq(&root).is_err());
        std::fs::remove_dir_all(&root).ok();
    }
}
