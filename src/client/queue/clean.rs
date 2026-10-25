//! Bulk queue cleanup behind `shunt clean` (design.md §3.9b).
//! Entries are never deleted in place: each is claimed by an
//! atomic rename into a per-run trash dir `<qroot>/.clean-<pid>/`,
//! which is removed once at the end. The rename is the race
//! recheck — an entry the daemon claimed first yields ENOENT and
//! counts as `skipped`, never a half-deleted `running/` entry.

use anyhow::{Context, Result};
use std::path::Path;

use super::list_entries;
use crate::proto::proc::{self, ProcProbe};

/// Which entry sets a `clean` run sweeps. `archive` means the
/// contents of `done/.archive/` and `failed/.archive/` — the §3.9
/// cap overflow, which no other verb reaches.
#[derive(Default, Clone, Copy)]
pub struct CleanSets {
    pub pending: bool,
    pub done: bool,
    pub failed: bool,
    pub archive: bool,
}

impl CleanSets {
    pub fn any(&self) -> bool {
        self.pending || self.done || self.failed || self.archive
    }
}

/// Per-set removal counts plus the races and failures met on the
/// way. `skipped` entries vanished mid-sweep (daemon claim or a
/// concurrent op); `errors` names entries that would not move.
#[derive(Default)]
pub struct CleanReport {
    pub pending: usize,
    pub done: usize,
    pub failed: usize,
    pub archive: usize,
    pub skipped: usize,
    pub errors: Vec<String>,
}

struct SetSweep {
    count: usize,
    skipped: usize,
    errors: Vec<String>,
}

/// Run one clean sweep over the selected sets. The trash dir is
/// dot-prefixed so no listing sees it; a kill -9 mid-sweep strands
/// it, and the next run removes stale `.clean-*` dirs first.
pub fn clean(root: &Path, sets: &CleanSets) -> CleanReport {
    let mut report = CleanReport::default();
    sweep_stale_trash(root, &mut report);
    let trash = root.join(format!(".clean-{}", std::process::id()));
    if sets.pending {
        report.pending = absorb(&mut report, sweep_set(root, "pending", &trash.join("pending")));
    }
    if sets.done {
        report.done = absorb(&mut report, sweep_set(root, "done", &trash.join("done")));
    }
    if sets.failed {
        report.failed = absorb(&mut report, sweep_set(root, "failed", &trash.join("failed")));
    }
    if sets.archive {
        report.archive = absorb(&mut report, sweep_set(root, "done/.archive", &trash.join("done.archive"))) + absorb(&mut report, sweep_set(root, "failed/.archive", &trash.join("failed.archive")));
    }
    if trash.exists() {
        if let Err(e) = std::fs::remove_dir_all(&trash) {
            // Claimed entries are already gone from every listing;
            // the residue is swept by the next run.
            report.errors.push(format!("cannot remove trash {}: {e:#}", trash.display()));
        }
    }
    report
}

/// Remove stale `.clean-*` dirs left by an interrupted run. They
/// hold only doomed entries, so removal needs no inspection — but
/// a `.clean-<pid>` whose pid is a LIVE `shunt clean` belongs to a
/// concurrent run, not a corpse: probe the pid first (Unknown is
/// treated live, the same fail-safe as §3.4 liveness).
fn sweep_stale_trash(root: &Path, report: &mut CleanReport) {
    let Ok(rd) = std::fs::read_dir(root) else { return };
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().into_owned();
        if !name.starts_with(".clean-") || !ent.path().is_dir() {
            continue;
        }
        let own = format!(".clean-{}", std::process::id());
        if name == own {
            // Same-pid residue: an earlier clean() call in this
            // process stranded its trash — that run is over.
        } else if let Ok(pid) = name[".clean-".len()..].parse::<u32>() {
            match proc::probe(pid, proc::read_cmdline, is_clean_cmdline) {
                ProcProbe::Dead => {}
                ProcProbe::Live | ProcProbe::Unknown => continue,
            }
        }
        if let Err(e) = std::fs::remove_dir_all(ent.path()) {
            report.errors.push(format!("cannot remove stale trash {name}: {e:#}"));
        }
    }
}

/// A live pid behind `.clean-<pid>` counts as a concurrent run
/// only when its argv carries a `clean` element — a recycled pid
/// running anything else is a dead run's dir by another name.
fn is_clean_cmdline(argv: &[&[u8]]) -> bool {
    argv.iter().any(|a| *a == b"clean")
}

/// Fold one set's sweep into the report, returning its count.
fn absorb(report: &mut CleanReport, s: SetSweep) -> usize {
    report.skipped += s.skipped;
    report.errors.extend(s.errors);
    s.count
}

/// Sweep every listed entry of `dir` into `trash_slot`.
fn sweep_set(root: &Path, dir: &str, trash_slot: &Path) -> SetSweep {
    let mut s = SetSweep { count: 0, skipped: 0, errors: Vec::new() };
    let names = match list_entries(root, dir) {
        Ok(n) => n,
        Err(e) => {
            s.errors.push(format!("{dir}: cannot list: {e:#}"));
            return s;
        }
    };
    if names.is_empty() {
        return s;
    }
    if let Err(e) = std::fs::create_dir_all(trash_slot) {
        s.errors.push(format!("{dir}: cannot create trash dir: {e:#}"));
        return s;
    }
    for name in names {
        match trash_entry(&root.join(dir), trash_slot, &name) {
            Ok(true) => s.count += 1,
            Ok(false) => s.skipped += 1,
            Err(e) => s.errors.push(format!("{dir}/{name}: {e:#}")),
        }
    }
    s
}

/// Move one entry — whatever its shape — into `trash`. Ok(false)
/// means the entry vanished first (claim/cancel race). A flat
/// `name.sh` entry's `name.sh.*` sidecars travel with it; orphan
/// sidecars whose body is already gone move too.
fn trash_entry(src: &Path, trash: &Path, name: &str) -> Result<bool> {
    let dir = src.join(name);
    if dir.is_dir() {
        return match std::fs::rename(&dir, trash.join(name)) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e).with_context(|| format!("cannot move {}", dir.display())),
        };
    }
    // Flat entry: collect `name.sh` plus every `name.sh.*` sibling
    // BEFORE renaming — a sidecar listing taken after the body move
    // could miss a sibling finalized in between.
    let body = format!("{name}.sh");
    let mut members = vec![body.clone()];
    if let Ok(rd) = std::fs::read_dir(src) {
        for ent in rd.flatten() {
            let fname = ent.file_name().to_string_lossy().into_owned();
            if fname.starts_with(&format!("{body}.")) {
                members.push(fname);
            }
        }
    }
    let mut moved = false;
    let mut first_err = None;
    for f in members {
        match std::fs::rename(src.join(&f), trash.join(&f)) {
            Ok(()) => moved = true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
    }
    match (moved, first_err) {
        (false, None) => Ok(false),
        (_, Some(e)) => Err(e).with_context(|| format!("cannot move {name}.sh*")),
        (true, None) => Ok(true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("shunt-cleantest-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("trash")).unwrap();
        dir
    }

    /// A name that is already gone (the daemon's claim or a
    /// concurrent cancel won) is a skip, not an error — the
    /// rename is the recheck.
    #[test]
    fn trash_entry_missing_name_is_skip() {
        let t = tmp("missing");
        assert!(!trash_entry(&t.join("src"), &t.join("trash"), "0100_x").unwrap());
        std::fs::remove_dir_all(&t).ok();
    }

    /// A flat entry moves its `name.sh.*` sidecars into trash with
    /// the body; an unrelated `name2.sh` is not a sibling.
    #[test]
    fn trash_entry_flat_takes_sidecars() {
        let t = tmp("flat");
        let src = t.join("src");
        std::fs::write(src.join("0100_x.sh"), "echo x\n").unwrap();
        std::fs::write(src.join("0100_x.sh.log"), "log\n").unwrap();
        std::fs::write(src.join("0100_x.sh.exit"), "0\n").unwrap();
        std::fs::write(src.join("0100_xy.sh"), "echo other\n").unwrap();
        assert!(trash_entry(&src, &t.join("trash"), "0100_x").unwrap());
        for f in ["0100_x.sh", "0100_x.sh.log", "0100_x.sh.exit"] {
            assert!(t.join("trash").join(f).is_file(), "{f} not in trash");
        }
        assert!(src.join("0100_xy.sh").is_file(), "unrelated entry moved");
        let leftovers: Vec<_> = std::fs::read_dir(&src).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(leftovers, vec!["0100_xy.sh".to_string()]);
        std::fs::remove_dir_all(&t).ok();
    }

    /// Orphan sidecars whose body is already gone still move —
    /// `Ok(true)`: residue of an entry counts as the entry removed.
    #[test]
    fn trash_entry_orphan_sidecars_move() {
        let t = tmp("orphan");
        let src = t.join("src");
        std::fs::write(src.join("0100_x.sh.log"), "log\n").unwrap();
        assert!(trash_entry(&src, &t.join("trash"), "0100_x").unwrap());
        assert!(t.join("trash").join("0100_x.sh.log").is_file());
        std::fs::remove_dir_all(&t).ok();
    }

    /// A dir entry moves whole — no recursive walk under a name
    /// the daemon could still be renaming into running/.
    #[test]
    fn trash_entry_dir_moves_atomically() {
        let t = tmp("dir");
        let src = t.join("src");
        std::fs::create_dir_all(src.join("0100_x")).unwrap();
        std::fs::write(src.join("0100_x/task.sh"), "echo x\n").unwrap();
        assert!(trash_entry(&src, &t.join("trash"), "0100_x").unwrap());
        assert!(t.join("trash").join("0100_x/task.sh").is_file());
        assert!(!src.join("0100_x").exists());
        std::fs::remove_dir_all(&t).ok();
    }
}
