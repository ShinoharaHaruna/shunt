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

/// Acquire the exclusive `.seq.lock` flock, retrying for up to
/// 30 s (matching `flock -w 30`). The returned File owns the hold —
/// dropping it releases the lock. `enqueue` holds it across
/// dedup-scan + mint + install so two same-key pushes serialize and
/// the second sees the first's entry.
fn seq_lock(root: &Path) -> Result<std::fs::File> {
    let lock_path = root.join(".seq.lock");
    let lock = std::fs::OpenOptions::new().create(true).append(true).open(&lock_path).with_context(|| format!("cannot open {}", lock_path.display()))?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match lock.try_lock_exclusive() {
            Ok(()) => return Ok(lock),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
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
fn mint_locked(root: &Path) -> Result<u32> {
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
/// the state machine's forward direction (pending → running →
/// done → failed): an entry claimed or finalized mid-scan moves
/// only into a track not yet scanned, so it cannot slip through
/// between scans. `.archive/` is out of scope — an entry delivered,
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

    /// A legacy flat claim normalizes inside pending/ and lands in
    /// running/ as a complete dir entry — the only state visible to
    /// running_list. At no point may running/<name> appear empty.
    #[test]
    fn claim_flat_normalizes_then_atomic_rename() {
        let root = tmp_root("flatclaim");
        std::fs::write(root.join("pending/0100_x.sh"), "echo hi\n").unwrap();
        assert!(claim(&root, "0100_x").unwrap());
        assert!(root.join("running/0100_x/task.sh").is_file());
        assert!(pending_list(&root).unwrap().is_empty());
        assert_eq!(running_list(&root).unwrap(), vec!["0100_x".to_string()]);
        // No staging litter may survive in pending/.
        let leftovers: Vec<_> = std::fs::read_dir(root.join("pending")).unwrap().flatten().collect();
        assert!(leftovers.is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    /// A flat entry whose dir-namespace is already held by a stray
    /// file is unclaimable (Err -> quarantine), never Ok(false).
    #[test]
    fn claim_flat_name_collision_is_err() {
        let root = tmp_root("flatcollide");
        std::fs::write(root.join("pending/0100_x.sh"), "echo hi\n").unwrap();
        std::fs::write(root.join("pending/0100_x"), "stray\n").unwrap();
        assert!(claim(&root, "0100_x").is_err());
        assert!(root.join("pending/0100_x.sh").exists());
        assert!(running_list(&root).unwrap().is_empty());
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

    /// A re-push after a lost ack carries the same `# shunt:id=`:
    /// the second enqueue must return the existing entry's name and
    /// mint nothing (§3.3). Bodies without the directive are
    /// distinct submissions — each mints its own number.
    #[test]
    fn enqueue_dedups_on_shunt_id() {
        let root = tmp_root("dedup");
        let body = "#!/usr/bin/env bash\n# shunt:id=abc-123\nexec true\n";
        let first = enqueue(&root, "t-abc123", body).unwrap();
        let EnqueueOutcome::Fresh(name) = first else {
            panic!("first push must mint");
        };
        match enqueue(&root, "t-abc123", body).unwrap() {
            EnqueueOutcome::Deduped(n) => assert_eq!(n, name),
            EnqueueOutcome::Fresh(n) => panic!("re-push minted {n}"),
        }
        assert_eq!(pending_list(&root).unwrap(), vec![name]);
        // No id -> no dedup, even for byte-identical bodies.
        let plain = "#!/usr/bin/env bash\nexec true\n";
        assert!(matches!(enqueue(&root, "x", plain).unwrap(), EnqueueOutcome::Fresh(_)));
        assert!(matches!(enqueue(&root, "x", plain).unwrap(), EnqueueOutcome::Fresh(_)));
        assert_eq!(pending_list(&root).unwrap().len(), 3);
        std::fs::remove_dir_all(&root).ok();
    }

    /// The dedup scan follows the state machine's forward direction:
    /// by retry time the original delivery may already sit in
    /// running/ or a terminal track — the re-push must still hit it.
    #[test]
    fn enqueue_dedup_finds_claimed_and_finished() {
        let root = tmp_root("deduptracks");
        for track in ["running", "done", "failed"] {
            let entry = root.join(track).join("0200_t");
            std::fs::create_dir_all(&entry).unwrap();
            std::fs::write(entry.join("task.sh"), "#!/usr/bin/env bash\n# shunt:id=zzz-9\nexec true\n").unwrap();
            match enqueue(&root, "t-zzz9", "#!/usr/bin/env bash\n# shunt:id=zzz-9\nexec true\n").unwrap() {
                EnqueueOutcome::Deduped(n) => assert_eq!(n, "0200_t"),
                EnqueueOutcome::Fresh(n) => panic!("entry in {track}/ missed by dedup; minted {n}"),
            }
            std::fs::remove_dir_all(&entry).unwrap();
        }
        std::fs::remove_dir_all(&root).ok();
    }

    /// Same-key pushes under `.seq.lock` serialize: exactly one
    /// mints, every other thread dedup-hits the same entry name.
    #[test]
    fn enqueue_concurrent_same_key_single_entry() {
        let root = tmp_root("deduprace");
        let body = "#!/usr/bin/env bash\n# shunt:id=race-1\nexec true\n";
        let mut handles = Vec::new();
        for _ in 0..8 {
            let r = root.clone();
            handles.push(std::thread::spawn(move || enqueue(&r, "t-race1", body).unwrap()));
        }
        let mut names = std::collections::BTreeSet::new();
        let mut minted = 0;
        for h in handles {
            let out = h.join().unwrap();
            minted += usize::from(!out.deduped());
            names.insert(out.name().to_string());
        }
        assert_eq!(minted, 1, "more than one push minted a number");
        assert_eq!(names.len(), 1, "threads disagreed on the entry: {names:?}");
        assert_eq!(pending_list(&root).unwrap().len(), 1);
        std::fs::remove_dir_all(&root).ok();
    }

    /// A corrupt `.seq` fails loudly (§3.2) — garbage, negative,
    /// and >9999 are all operator damage, never silently reseeded.
    /// A missing `.seq` is fine: the high-water floor rebuilds it.
    #[test]
    fn next_seq_corrupt_counter_bails() {
        let root = tmp_root("corruptseq");
        for bad in ["garbage", "-5", "10000", "99999999", ""] {
            std::fs::write(root.join(".seq"), bad).unwrap();
            let e = next_seq(&root).unwrap_err().to_string();
            assert!(e.contains("corrupt"), "{bad:?} should bail as corrupt, got: {e}");
        }
        std::fs::remove_file(root.join(".seq")).unwrap();
        assert_eq!(next_seq(&root).unwrap(), SEQ_INIT + 1);
        std::fs::remove_dir_all(&root).ok();
    }
}
