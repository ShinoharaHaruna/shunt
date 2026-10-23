//! Enqueue install: `# shunt:id=` dedup scan + seq mint + atomic
//! pending/ entry creation, all under one `.seq.lock` hold
//! (design.md §3.3).

use anyhow::{Context, Result};
use std::path::Path;

use super::entries::{list_entries, task_file};
use super::seq::{mint_locked, seq_lock};

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

/// Scan the four live tracks for an entry whose task.sh head
/// carries `# shunt:id=<key>`; first hit wins. Scan order follows
/// forward transitions (pending → running → done → failed), so a
/// claim or finalize moves into a not-yet-scanned track. Reschedule
/// moves backward (running → pending) and must share `.seq.lock`
/// with this scan to prevent an entry slipping behind it.
/// `.archive/` is out of scope — an entry delivered,
/// finished AND archived inside one retry window is pathological,
/// and a missed dedup there degrades to a guard-absorbed rerun.
/// Runs under `.seq.lock` (callers), so no same-key push can
/// interleave between this scan and the install.
fn dedup_lookup(root: &Path, key: &str) -> Result<Option<String>> {
    use std::io::Read;
    for track in ["pending", "running", "done", "failed"] {
        for name in list_entries(root, track)? {
            let tf = task_file(root, track, &name);
            // Directives live in the comment head; 8 KiB covers any
            // plausible head without reading a whole body.
            let mut buf = Vec::new();
            let found = match std::fs::File::open(&tf).and_then(|f| f.take(8192).read_to_end(&mut buf)) {
                Ok(_) => crate::proto::task::dedup_key(&String::from_utf8_lossy(&buf)).is_some_and(|k| k == key),
                // Entry vanished mid-scan (cancel/claim race): not a
                // dedup target anymore. Any other read failure is a
                // real fault — skipping it could silently mint a
                // duplicate, so it fails the enqueue instead.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
                Err(e) => return Err(e).with_context(|| format!("cannot read {}", tf.display())),
            };
            if found {
                return Ok(Some(name));
            }
        }
    }
    Ok(None)
}

/// Install a task body into pending/ as `NNNN_<slug>/task.sh`, mode
/// 0755, via write-temp-dir-then-mv inside pending/ (atomic same-fs
/// rename). A body carrying `# shunt:id=` that already exists in a
/// live track returns `Deduped` with the existing name — a re-push
/// after a lost ack never mints a second number (design.md §3.3).
pub fn enqueue(root: &Path, slug: &str, body: &str) -> Result<EnqueueOutcome> {
    use std::os::unix::fs::PermissionsExt;
    // One critical section — dedup-scan, mint, install — under a
    // single `.seq.lock` hold, so concurrent same-key pushes
    // serialize and the loser dedup-hits the winner's entry.
    let _lock = seq_lock(root)?;
    if let Some(key) = crate::proto::task::dedup_key(body) {
        if let Some(name) = dedup_lookup(root, &key)? {
            return Ok(EnqueueOutcome::Deduped(name));
        }
    }
    let n = mint_locked(root)?;
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
    Ok(EnqueueOutcome::Fresh(name))
}
