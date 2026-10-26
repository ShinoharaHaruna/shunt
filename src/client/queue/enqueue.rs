//! Enqueue install: indexed `# shunt:id=` dedup + seq mint + atomic
//! pending/ entry creation, all under one `.seq.lock` hold
//! (design.md §3.3).

use anyhow::{Context, Result};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use super::entries::{list_entries, task_file};
use super::seq::{mint_locked, seq_lock};

/// Revalidate stale index rows once per 64 KiB of appended growth.
/// This amortizes the stats over many submissions while keeping stale
/// history bounded even when the live index itself exceeds the threshold.
const DEDUP_COMPACT_BYTES: u64 = 64 * 1024;

#[cfg(test)]
std::thread_local! {
    pub(super) static DEDUP_COMPACT_BYTES_OVERRIDE: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

pub(super) fn dedup_compact_bytes() -> u64 {
    #[cfg(test)]
    if let Some(bytes) = DEDUP_COMPACT_BYTES_OVERRIDE.with(std::cell::Cell::get) {
        return bytes.max(1);
    }
    DEDUP_COMPACT_BYTES
}

/// What `enqueue` did with a body: minted a fresh entry, or
/// recognized a re-pushed delivery by its `# shunt:id=` key and
/// returned the existing one.
pub enum EnqueueOutcome {
    Fresh(String),
    Deduped(String),
}

impl EnqueueOutcome {
    pub fn name(&self) -> &str {
        match self {
            Self::Fresh(n) | Self::Deduped(n) => n,
        }
    }

    pub fn deduped(&self) -> bool {
        matches!(self, Self::Deduped(_))
    }
}

/// Read a task's directive from the comment head, without loading
/// the body. Missing entries are races; other read failures are real
/// faults because skipping one could mint a duplicate.
fn entry_dedup_key(path: &Path) -> Result<Option<String>> {
    let mut buf = Vec::new();
    match std::fs::File::open(path).and_then(|f| f.take(8192).read_to_end(&mut buf)) {
        Ok(_) => Ok(crate::proto::task::dedup_key(&String::from_utf8_lossy(&buf))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

/// The pre-index behavior, retained for one-time bootstrap of queues
/// created by older binaries. Track order matches forward transitions.
fn dedup_scan(root: &Path) -> Result<Vec<(String, String)>> {
    let mut entries = Vec::new();
    for track in ["pending", "running", "done", "failed"] {
        for name in list_entries(root, track)? {
            let tf = task_file(root, track, &name);
            if let Some(key) = entry_dedup_key(&tf)? {
                entries.push((key, name));
            }
        }
    }
    Ok(entries)
}

fn temp_index_path(root: &Path, purpose: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
    root.join(format!(".dedup.{purpose}.{}.{nanos}", std::process::id()))
}

/// Replace the index atomically so an interrupted bootstrap or
/// compaction leaves the previous complete index available.
fn replace_dedup_index(root: &Path, contents: &[u8], purpose: &str) -> Result<()> {
    let path = root.join(".dedup");
    let tmp = temp_index_path(root, purpose);
    let result = (|| -> Result<()> {
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp).with_context(|| format!("cannot create {}", tmp.display()))?;
        f.write_all(contents).with_context(|| format!("cannot write {}", tmp.display()))?;
        f.flush().with_context(|| format!("cannot flush {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("cannot replace {}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Return whether this name still resolves to a live dir entry or a
/// legacy flat `.sh` entry. Archives are intentionally outside the
/// dedup lifetime, like the original scan.
fn live_entry_exists(root: &Path, name: &str) -> Result<bool> {
    if name.is_empty() || Path::new(name).file_name().and_then(|n| n.to_str()) != Some(name) {
        return Ok(false);
    }
    // New entries take one direct name lookup per possible track; the
    // second pass is only for legacy flat files.
    for track in ["pending", "running", "done", "failed"] {
        let dir = root.join(track).join(name);
        match std::fs::symlink_metadata(&dir) {
            Ok(meta) if meta.file_type().is_dir() => return Ok(true),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("cannot inspect {}", dir.display())),
        }
    }
    for track in ["pending", "running", "done", "failed"] {
        let flat = root.join(track).join(format!("{name}.sh"));
        match std::fs::symlink_metadata(&flat) {
            Ok(meta) if meta.file_type().is_file() => return Ok(true),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("cannot inspect {}", flat.display())),
        }
    }
    Ok(false)
}

/// Keep only rows whose names still resolve on a live track. This
/// preserves append order, so the last row for a re-minted key wins.
fn compact_dedup_index(root: &Path) -> Result<()> {
    let path = root.join(".dedup");
    let contents = std::fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
    let mut retained = String::new();
    for line in contents.lines() {
        let Some((key, name)) = line.split_once('\t') else { continue };
        if !key.is_empty() && live_entry_exists(root, name)? {
            retained.push_str(key);
            retained.push('\t');
            retained.push_str(name);
            retained.push('\n');
        }
    }
    replace_dedup_index(root, retained.as_bytes(), "compact")
}

fn bootstrap_dedup_index(root: &Path) -> Result<Vec<(String, String)>> {
    let entries = dedup_scan(root)?;
    let mut contents = String::new();
    for (key, name) in &entries {
        contents.push_str(key);
        contents.push('\t');
        contents.push_str(name);
        contents.push('\n');
    }
    replace_dedup_index(root, contents.as_bytes(), "bootstrap")?;
    if contents.len() as u64 >= dedup_compact_bytes() {
        compact_dedup_index(root)?;
    }
    Ok(entries)
}

/// Read `.dedup` and return the last live name for `key`. A missing
/// index gets one legacy full scan and an atomic bootstrap; a stale
/// last row is a miss, matching the previous behavior after removal.
fn dedup_lookup(root: &Path, key: &str) -> Result<Option<String>> {
    let path = root.join(".dedup");
    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let entries = bootstrap_dedup_index(root)?;
            // Preserve the full scan's first-hit behavior on this one
            // call; subsequent lookups use the append-order index.
            return Ok(entries.into_iter().find(|(indexed_key, _)| indexed_key == key).map(|(_, name)| name));
        }
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    };
    let name = contents.lines().filter_map(|line| line.split_once('\t')).filter(|(indexed_key, _)| *indexed_key == key).map(|(_, name)| name).next_back();
    let Some(name) = name else { return Ok(None) };
    if live_entry_exists(root, name)? {
        Ok(Some(name.to_string()))
    } else {
        Ok(None)
    }
}

/// Append an index row before publishing the pending dir. If install
/// fails the row is stale and ignored; writing first prevents a crash
/// after the atomic rename from leaving a live delivery unindexed.
fn append_dedup_index(root: &Path, key: &str, name: &str) -> Result<(u64, u64)> {
    let path = root.join(".dedup");
    let old_len = std::fs::metadata(&path).with_context(|| format!("cannot inspect {}", path.display()))?.len();
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path).with_context(|| format!("cannot append {}", path.display()))?;
    writeln!(f, "{key}\t{name}").with_context(|| format!("cannot append {}", path.display()))?;
    f.flush().with_context(|| format!("cannot flush {}", path.display()))?;
    let new_len = f.metadata().with_context(|| format!("cannot inspect {}", path.display()))?.len();
    Ok((old_len, new_len))
}

fn crossed_compaction_boundary(old_len: u64, new_len: u64) -> bool {
    let threshold = dedup_compact_bytes();
    new_len >= threshold && old_len / threshold < new_len / threshold
}

/// Name-changing queue moves invalidate the name-based index under
/// `.seq.lock`; the next lookup bootstraps against current live names.
pub(crate) fn invalidate_dedup_index(root: &Path) -> Result<()> {
    let path = root.join(".dedup");
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("cannot invalidate {}", path.display())),
    }
}

/// Install an unheld task body into pending/ as `NNNN_<slug>/task.sh`.
pub fn enqueue(root: &Path, slug: &str, body: &str) -> Result<EnqueueOutcome> {
    enqueue_with_hold(root, slug, body, None)
}

/// Install a task body and optional admission marker before publishing
/// the pending entry. This avoids a window where a requested hold is
/// visible as ordinary claimable work.
pub fn enqueue_with_hold(root: &Path, slug: &str, body: &str, hold: Option<&str>) -> Result<EnqueueOutcome> {
    use std::os::unix::fs::PermissionsExt;
    // One critical section serializes retries and keeps same-key
    // enqueue, mint, index append, and install in a single order.
    let _lock = seq_lock(root)?;
    let dedup_key = crate::proto::task::dedup_key(body);
    if let Some(key) = &dedup_key {
        if let Some(name) = dedup_lookup(root, key)? {
            return Ok(EnqueueOutcome::Deduped(name));
        }
    }
    let n = mint_locked(root)?;
    let name = format!("{n:04}_{slug}");
    let pending = root.join("pending");
    std::fs::create_dir_all(&pending)?;
    let tmp = pending.join(format!(".tmp.{}.{}", std::process::id(), n));
    std::fs::create_dir_all(&tmp)?;
    let mut index_growth = None;
    let result = (|| -> Result<()> {
        let tf = tmp.join("task.sh");
        std::fs::write(&tf, body)?;
        std::fs::set_permissions(&tf, std::fs::Permissions::from_mode(0o755))?;
        if let Some(reason) = hold {
            std::fs::write(tmp.join("held"), reason)?;
        }
        if let Some(key) = &dedup_key {
            index_growth = Some(append_dedup_index(root, key, &name)?);
        }
        std::fs::rename(&tmp, pending.join(&name))?;
        Ok(())
    })();
    // Any failure after the staging dir exists must not strand a
    // `.tmp.*` dir in pending/ (dot-prefixed, so listings skip it).
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&tmp);
    }
    result?;
    if let Some((old_len, new_len)) = index_growth {
        if crossed_compaction_boundary(old_len, new_len) {
            compact_dedup_index(root)?;
        }
    }
    Ok(EnqueueOutcome::Fresh(name))
}
