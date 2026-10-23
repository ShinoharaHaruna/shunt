//! Sequence minting: the `.seq` counter and the `.seq.lock` flock
//! that serializes dedup-scan + mint + install (design.md §3.2–3.3).

use anyhow::{Context, Result};
use fs2::FileExt;
use std::path::Path;
use std::time::{Duration, Instant};

use super::entries::list_entries;
use super::{PRIORITY_BAND_MAX, SEQ_INIT, SEQ_MAX};

#[cfg(test)]
std::thread_local! {
    // A one-shot observer makes lock contention testable without timing guesses.
    pub(super) static ON_SEQ_LOCK_WAIT: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

/// Acquire the exclusive `.seq.lock` flock, retrying for up to
/// 30 s (matching `flock -w 30`). The returned File owns the hold —
/// dropping it releases the lock. `enqueue` holds it across
/// dedup-scan + mint + install so two same-key pushes serialize and
/// the second sees the first's entry. Rescheduling shares this hold
/// across its backward running → pending mutation so dedup cannot
/// miss an entry moving behind its scan. Rooms (design.md §17) reuse
/// the same `<dir>/.seq.lock` hold for their message counter.
pub fn seq_lock(root: &Path) -> Result<std::fs::File> {
    let lock_path = root.join(".seq.lock");
    let lock = std::fs::OpenOptions::new().create(true).append(true).open(&lock_path).with_context(|| format!("cannot open {}", lock_path.display()))?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match lock.try_lock_exclusive() {
            Ok(()) => return Ok(lock),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                #[cfg(test)]
                ON_SEQ_LOCK_WAIT.with(|observer| {
                    if let Some(observer) = observer.borrow_mut().take() {
                        observer();
                    }
                });
                if Instant::now() >= deadline {
                    anyhow::bail!("timed out waiting for .seq.lock");
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => return Err(e).context("flock .seq.lock failed"),
        }
    }
}

/// Read `.seq`, mint the next number, write it back. Caller holds
/// `.seq.lock`. A missing `.seq` seeds at SEQ_INIT (the high-water
/// floor below still keeps the mint behind every queued entry);
/// a present-but-unreadable counter — garbage, negative, or past
/// the 4-digit ceiling — bails loudly instead of silently
/// reseeding over operator intent (design.md §3.2).
pub(super) fn mint_locked(root: &Path) -> Result<u32> {
    let seq_path = root.join(".seq");
    let cur: u32 = match std::fs::read_to_string(&seq_path) {
        Ok(s) => s.trim().parse::<u32>().ok().filter(|&v| v <= SEQ_MAX).with_context(|| format!("{} is corrupt (contains {:?}); remove it to rebuild from the queue's high-water mark, or write a valid 0-{SEQ_MAX} counter", seq_path.display(), s.trim()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => SEQ_INIT,
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", seq_path.display())),
    };
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
    Ok(next)
}

/// Read + increment `.seq` under an flock on `.seq.lock`, retrying
/// for up to 30 s (matching `flock -w 30`).
pub fn next_seq(root: &Path) -> Result<u32> {
    let _lock = seq_lock(root)?;
    mint_locked(root)
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
