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

/// Liveness identity string for cmdline matching (design.md §3.4).
/// Single-sourced here; the bash version duplicated it in three
/// scripts, which was a logged defect.
pub const DAEMON_IDENTITY: &str = "shunt daemon";

/// Built-in pool size when neither the file nor env supplies one.
pub const DEFAULT_WORKERS: u32 = 3;

/// Default idle-shutdown grace (design.md §3.11).
pub const DEFAULT_GRACE_S: u64 = 60;

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
        std::fs::create_dir_all(root.join(d))
            .with_context(|| format!("cannot create {}", root.join(d).display()))?;
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
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&lock_path)
        .with_context(|| format!("cannot open {}", lock_path.display()))?;
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
    let cur: u32 = std::fs::read_to_string(&seq_path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(SEQ_INIT);
    let next = cur + 1;
    std::fs::write(&seq_path, format!("{next}\n"))?;
    lock.unlock()?;
    Ok(next)
}

/// Filenames of `dir/*.sh` (or another suffix) in `LC_ALL=C` byte sort
/// order. For pending this is the FIFO claim order.
pub fn list_sh(root: &Path, track: &str) -> Result<Vec<String>> {
    let mut names = Vec::new();
    let dir = root.join(track);
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

/// Pending filenames in claim order.
pub fn pending_list(root: &Path) -> Result<Vec<String>> {
    list_sh(root, "pending")
}

/// Running task basenames (running/*.sh), sorted.
pub fn running_list(root: &Path) -> Result<Vec<String>> {
    list_sh(root, "running")
}

/// Pending tasks whose filename contains `pat`, sorted.
pub fn match_pending(root: &Path, pat: &str) -> Result<Vec<String>> {
    Ok(pending_list(root)?
        .into_iter()
        .filter(|n| n.contains(pat))
        .collect())
}

/// Running task basenames (no `.pid`) whose `<task>.sh.pid` filename
/// contains `pat`, sorted.
pub fn match_running(root: &Path, pat: &str) -> Result<Vec<String>> {
    let mut names = Vec::new();
    let dir = root.join("running");
    match std::fs::read_dir(&dir) {
        Ok(rd) => {
            for ent in rd {
                let name = ent?.file_name().to_string_lossy().into_owned();
                if name.ends_with(".sh.pid") && name.contains(pat) {
                    names.push(name.trim_end_matches(".pid").to_string());
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", dir.display())),
    }
    names.sort();
    Ok(names)
}

/// Install a task body into pending/ as `NNNN_<slug>.sh`, mode 0755,
/// via write-temp-then-mv inside pending/ (atomic same-fs rename).
/// Returns the installed filename.
pub fn enqueue(root: &Path, slug: &str, body: &str) -> Result<String> {
    use std::os::unix::fs::PermissionsExt;
    let n = next_seq(root)?;
    let name = format!("{n:04}_{slug}.sh");
    let pending = root.join("pending");
    std::fs::create_dir_all(&pending)?;
    let tmp = pending.join(format!(".tmp.{}.{}", std::process::id(), n));
    std::fs::write(&tmp, body)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&tmp, pending.join(&name))?;
    Ok(name)
}

/// Recent finished tasks in a track: `(task name, rc)` newest first.
pub fn recent_exits(root: &Path, track: &str, n: usize) -> Vec<(String, i32)> {
    let dir = root.join(track);
    let mut rows: Vec<(SystemTime, String)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for ent in rd.flatten() {
            let name = ent.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".exit") {
                continue;
            }
            let mtime = ent
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(UNIX_EPOCH);
            rows.push((mtime, name));
        }
    }
    rows.sort_by_key(|r| std::cmp::Reverse(r.0));
    rows.truncate(n);
    rows.into_iter()
        .map(|(_, f)| {
            let rc = std::fs::read_to_string(dir.join(&f))
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(-1);
            (f.trim_end_matches(".exit").to_string(), rc)
        })
        .collect()
}

/// RFC 3339 UTC timestamp (`date -Is` equivalent).
pub fn iso8601_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
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
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}+00:00",
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60
    )
}

/// Seconds since the unix epoch.
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Extract digits from a string (`tr -cd '0-9'` semantics); `None`
/// when nothing or the value overflows.
pub fn digits_only(s: &str) -> Option<u32> {
    let d: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
    d.parse().ok()
}
