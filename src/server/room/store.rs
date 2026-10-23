//! Filesystem layer: `rooms/` dirs, name resolution, meta.json IO,
//! the `.seq` mint, reader cursors, and exclusive room init.

use crate::client::queue::seq_lock;
use crate::server::fsutil::{atomic_write, private_dir, tmp_sibling, write_private};
use crate::server::registry::names::{self, Resolution};
use crate::server::registry::{shunt_home, valid_name};
use anyhow::{Context, Result};
use std::fs;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use super::{readers_max, Meta, Reader};

/// Live stream is a 4-digit namespace like the queue's (§17.1
/// defers to the §3.2 rules). Unlike the queue there is no
/// priority band, so the counter starts at 0 and `0001` is the
/// first message.
const SEQ_INIT: u32 = 0;
const SEQ_MAX: u32 = 9999;

/// Cold-storage namespace (§17): `rooms/.archive/` holds whole
/// room dirs — a room dir is fully self-contained (meta.json,
/// msgs/ incl. msgs/.archive, readers/, .seq), so one rename moves
/// the entire record between live and archived with no format
/// work. Dot-prefixed, so `room_names` keeps the live candidate
/// set disjoint from the archive for free.
pub(super) const ARCHIVE_DIR: &str = ".archive";

/// `$SHUNT_HOME/rooms/`, created 0700 like every registry dir.
pub(super) fn rooms_dir() -> Result<PathBuf> {
    let d = shunt_home()?.join("rooms");
    private_dir(&d)?;
    Ok(d)
}

/// The rooms dir only if it already exists — pure read paths (`ls`,
/// `status`) must not create registry dirs just by polling.
pub(super) fn existing_rooms_dir() -> Option<PathBuf> {
    let d = shunt_home().ok()?.join("rooms");
    d.is_dir().then_some(d)
}

/// Room dir names, sorted — the candidate set lives in
/// `registry::names` so listing and resolving share one predicate.
pub(super) fn room_names(dir: &Path) -> Vec<String> {
    names::room_names(dir)
}

/// The shared live-miss error — `resolve` and `remove` each run
/// their own archived-name check first, then build this identically
/// so the wording never drifts between them.
pub(super) fn unknown_room_err(input: &str, suggestions: &[String]) -> anyhow::Error {
    anyhow::anyhow!("no room named or prefixed {input:?}{} — `shunt room ls --all` lists what exists, `shunt room create <name> --members p1,p2` makes one", names::did_you_mean(suggestions))
}

/// `<room>` resolves through the name middleware (§4.1): exact,
/// then unique case-fold or unique prefix (§17.2's accepted
/// resolutions, both reported as `Canonical` and taken silently),
/// ambiguous prefix a loud bail. The input must be a valid name
/// regardless — it is joined under rooms/, so `..`/slashes can
/// never escape the dir.
pub(super) fn resolve(input: &str) -> Result<(String, PathBuf)> {
    valid_name(input)?;
    let dir = rooms_dir()?;
    match names::resolve_room(input)? {
        Resolution::Exact(name) | Resolution::Canonical { canonical: name, .. } => Ok((name.clone(), dir.join(&name))),
        Resolution::Unknown { input, suggestions } => {
            // An exact archived namesake is not "unknown" — the
            // error names the restore path instead. Exact-only:
            // cold storage names are typed in full, never
            // prefix-matched (the prefix ladder stays live-only).
            if dir.join(ARCHIVE_DIR).join(&input).is_dir() {
                anyhow::bail!("room '{input}' is archived — `shunt room unarchive {input}` restores it");
            }
            Err(unknown_room_err(&input, &suggestions))
        }
        Resolution::Ambiguous(cands) => anyhow::bail!("room prefix {input:?} is ambiguous: {}", cands.join(", ")),
    }
}

pub(super) fn load_meta(dir: &Path) -> Result<Meta> {
    let path = dir.join("meta.json");
    let text = fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("bad JSON in {} — fix or remove the file; `room rm` drops the whole room", path.display()))
}

/// meta.json rewrite via fsutil::atomic_write — a concurrent
/// `ls`/`members` must never read a torn roster.
pub(super) fn save_meta(dir: &Path, meta: &Meta) -> Result<()> {
    atomic_write(&dir.join("meta.json"), serde_json::to_string_pretty(meta)?)
}

/// Sorted NNNN files under one message dir (live `msgs/` or
/// `msgs/.archive/` — same numbering namespace).
pub(super) fn msg_numbers_in(msgs_dir: &Path) -> Vec<u32> {
    let mut ns = Vec::new();
    if let Ok(rd) = fs::read_dir(msgs_dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if let Some(n) = name.strip_suffix(".json").and_then(|s| s.parse::<u32>().ok()).filter(|_| name.len() == "0000.json".len()) {
                ns.push(n);
            }
        }
    }
    ns.sort_unstable();
    ns
}

/// Live message numbers, sorted. `msgs/.archive/` is dot-prefixed
/// and excluded — `read`/`ls` never see archived messages.
pub(super) fn msg_numbers(dir: &Path) -> Vec<u32> {
    msg_numbers_in(&dir.join("msgs"))
}

/// Highest NNNN anywhere in the message namespace — live msgs AND
/// .archive (the §3.2 floor spans archives too: a wrapped counter
/// minting onto an archived number collides there).
fn msg_high_water(dir: &Path) -> u32 {
    let mut hi = SEQ_INIT;
    let mut bump = |name: &std::ffi::OsString| {
        let s = name.to_string_lossy();
        if let Some(v) = s.strip_suffix(".json").and_then(|p| p.parse::<u32>().ok()).filter(|_| s.len() == "0000.json".len()) {
            hi = hi.max(v);
        }
    };
    if let Ok(rd) = fs::read_dir(dir.join("msgs")) {
        for e in rd.flatten() {
            bump(&e.file_name());
        }
    }
    if let Ok(rd) = fs::read_dir(dir.join("msgs/.archive")) {
        for e in rd.flatten() {
            bump(&e.file_name());
        }
    }
    hi
}

/// Read `.seq`, mint the next number, write it back — the §3.2
/// discipline mirrored for the message namespace. Caller holds
/// `.seq.lock`. A missing `.seq` rebuilds from the high-water mark
/// (repairable by deletion); a present-but-unparseable counter is
/// operator damage and fails with remediation, never silently
/// reseeded.
pub(super) fn mint_locked(dir: &Path) -> Result<u32> {
    let seq_path = dir.join(".seq");
    let cur: u32 = match fs::read_to_string(&seq_path) {
        Ok(s) => s.trim().parse::<u32>().ok().filter(|&v| v <= SEQ_MAX).with_context(|| format!("{} is corrupt (contains {:?}); remove it to rebuild from the stream's high-water mark, or write a valid 0-{SEQ_MAX} counter", seq_path.display(), s.trim()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => SEQ_INIT,
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", seq_path.display())),
    };
    let hi = msg_high_water(dir);
    let next = if cur >= SEQ_MAX {
        // Counter wrapped the 4-digit ceiling: continue just past
        // the highest occupied number.
        hi + 1
    } else {
        (cur + 1).max(hi + 1)
    };
    if next > SEQ_MAX {
        anyhow::bail!("message sequence space exhausted: msgs/ already holds {SEQ_MAX}; `room rm` the room or prune msgs/.archive first");
    }
    write_private(&seq_path, format!("{next}\n"))?;
    Ok(next)
}

/// A label's high-water mark; absent/unreadable file = never read.
pub(super) fn last_read(dir: &Path, label: &str) -> u32 {
    fs::read_to_string(dir.join("readers").join(format!("{label}.json"))).ok().and_then(|t| serde_json::from_str::<Reader>(&t).ok()).map(|r| r.last_read_n).unwrap_or(0)
}

/// `unread_msgs` for a label: count of live NNNN past the mark
/// (§17.5 — one pass over the linear stream).
pub(super) fn unread_count(dir: &Path, label: &str) -> usize {
    let mark = last_read(dir, label);
    msg_numbers(dir).into_iter().filter(|&n| n > mark).count()
}

/// A read supplies its returned high-water mark; say/decide acknowledge
/// the current live maximum. Serialize the read-modify-write and eviction
/// so an older shared-label caller cannot regress a newer cursor or race
/// on the same temporary file. Message loading stays outside the lock.
pub(super) fn mark(dir: &Path, label: &str, through: Option<u32>) -> Result<()> {
    let _lock = seq_lock(dir)?;
    let _ = fs::set_permissions(dir.join(".seq.lock"), fs::Permissions::from_mode(0o600));
    let max_n = through.unwrap_or_else(|| msg_numbers(dir).last().copied().unwrap_or(0)).max(last_read(dir, label));
    let rdir = dir.join("readers");
    private_dir(&rdir)?;
    atomic_write(&rdir.join(format!("{label}.json")), serde_json::to_string(&Reader { last_read_n: max_n })?)?;

    let mut files: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    if let Ok(rd) = fs::read_dir(&rdir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || !name.ends_with(".json") {
                continue;
            }
            let mtime = e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
            files.push((mtime, e.path()));
        }
    }
    if files.len() > readers_max() {
        files.sort_by_key(|(t, p)| (*t, p.clone()));
        for (_, p) in files.iter().take(files.len() - readers_max()) {
            let _ = fs::remove_file(p);
        }
    }
    Ok(())
}

pub(super) fn initialize_room(dir: &Path, initialize: impl FnOnce(&Path) -> Result<()>) -> Result<()> {
    // mkdir is the ownership boundary: even an empty pre-existing directory
    // belongs to somebody else and must not be initialized or chmodded.
    fs::DirBuilder::new().mode(0o700).create(dir).with_context(|| format!("cannot create room {} exclusively — if it already exists, use `room members` or `room rm`", dir.display()))?;
    let owned = fs::symlink_metadata(dir)?;
    let result = fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(anyhow::Error::from).and_then(|()| initialize(dir));
    if result.is_err() {
        // Never recursively delete: a replacement or unexpected content must
        // survive a failed initializer. Inode identity also protects an empty
        // replacement, which remove_dir alone would otherwise accept.
        if fs::symlink_metadata(dir).is_ok_and(|m| m.dev() == owned.dev() && m.ino() == owned.ino()) {
            let cleanup = || -> std::io::Result<()> {
                // save_meta's staging sibling, if it stranded mid-write.
                let tmp = tmp_sibling(&dir.join("meta.json"));
                if tmp.exists() {
                    fs::remove_file(tmp)?;
                }
                for child in ["msgs", "readers"] {
                    match fs::remove_dir(dir.join(child)) {
                        Ok(()) => (),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                        Err(e) => return Err(e),
                    }
                }
                fs::remove_dir(dir)
            };
            if let Err(error) = cleanup() {
                return result.with_context(|| format!("partial room {} retained: cleanup failed: {error}; inspect before retrying", dir.display()));
            }
        }
    }
    result
}
