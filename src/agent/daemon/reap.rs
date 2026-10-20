//! Orphan requeue on daemon startup (design.md §3.6).

use anyhow::Result;
use serde_json::json;
use std::path::Path;

use super::{dlog, ev};
use crate::agent::{exec, queue};

/// Orphan requeue: entries left in running/ by a dead daemon go to
/// `pending/0000_requeued_<clean>`; the interrupted log is swept to
/// `failed/<clean>.interrupted.<ts>.log` (design.md §3.6). Dir and
/// legacy flat entries are both handled.
pub(crate) fn requeue_orphans(root: &Path) -> Result<()> {
    let running = root.join("running");
    let pending = root.join("pending");
    let failed = root.join("failed");
    for base in queue::running_list(root)? {
        if let Some((pgid, start)) = exec::read_pid_file(root, "running", &base) {
            // A recorded start_time that no longer matches means the
            // pgid was recycled — the leftover group is a stranger's,
            // so no warning (and nothing is ever signaled here).
            let ours = start.is_none_or(|s| exec::proc_pgrp_start(pgid).map(|(_, st)| st == s).unwrap_or(false));
            if ours && exec::pgid_alive(pgid) {
                dlog(
                    root,
                    "orphan_live_pgid",
                    ev(json!({"task": base, "pgid": pgid,
                           "warning": "process group still alive; requeued anyway, rerun should be a no-op via the task guard"})),
                );
            }
        }
        let mut clean = base.as_str();
        while let Some(rest) = clean.strip_prefix("0000_requeued_") {
            clean = rest;
        }
        if queue::is_dir_entry(root, "running", &base) {
            let dir = running.join(&base);
            let old_log = dir.join("log");
            if old_log.exists() {
                // Not `with_extension("log")`: the timestamp after the
                // last dot IS the extension, so it must go inside the
                // name — `<clean>.interrupted.<unixts>.log` (§3.6).
                let _ = std::fs::rename(&old_log, failed.join(format!("{clean}.interrupted.{}.log", queue::unix_now())));
            }
            let _ = std::fs::remove_file(dir.join("pid"));
            // Rust rename() onto an existing EMPTY dir silently
            // replaces it (unlike bash mv) — check the target first
            // and take the same quarantine path as a rename failure.
            let target = pending.join(format!("0000_requeued_{clean}"));
            let moved = if target.exists() { Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, format!("target exists: {}", target.display()))) } else { std::fs::rename(&dir, &target) };
            if let Err(e) = moved {
                // A failed rename must not strand the entry in
                // running/ forever: no pid file means kill/status
                // can't see it. Quarantine to failed/ so the task
                // stays accounted for (design.md §3.6).
                dlog(root, "requeue_failed", ev(json!({"task": base, "error": e.to_string()})));
                if let Err(e2) = exec::quarantine(root, "running", &base, &format!("orphan requeue failed: {e}")) {
                    dlog(
                        root,
                        "ghost_entry",
                        ev(json!({"task": base, "error": e2.to_string(),
                           "warning": "entry stuck in running/ with no pid file; kill cannot reach it"})),
                    );
                }
                continue;
            }
        } else {
            // Legacy flat entry: stays a flat `NNNN_slug.sh` in
            // pending so the listing keeps seeing it.
            let target = pending.join(format!("0000_requeued_{clean}.sh"));
            let moved = if target.exists() { Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, format!("target exists: {}", target.display()))) } else { std::fs::rename(running.join(format!("{base}.sh")), &target) };
            if let Err(e) = moved {
                dlog(root, "requeue_failed", ev(json!({"task": base, "error": e.to_string()})));
                if let Err(e2) = exec::quarantine(root, "running", &base, &format!("orphan requeue failed: {e}")) {
                    dlog(
                        root,
                        "ghost_entry",
                        ev(json!({"task": base, "error": e2.to_string(),
                           "warning": "entry stuck in running/ with no pid file; kill cannot reach it"})),
                    );
                }
                continue;
            }
            let old_log = running.join(format!("{base}.sh.log"));
            if old_log.exists() {
                let _ = std::fs::rename(&old_log, failed.join(format!("{clean}.interrupted.{}.log", queue::unix_now())));
            }
            let _ = std::fs::remove_file(running.join(format!("{base}.sh.pid")));
        }
        dlog(root, "requeue_orphan", ev(json!({"from": base, "to": format!("0000_requeued_{clean}")})));
    }
    // sweep legacy strays: .pid/.sh.log files with no entry left
    if let Ok(rd) = std::fs::read_dir(&running) {
        for ent in rd.flatten() {
            let name = ent.file_name().to_string_lossy().into_owned();
            if name.ends_with(".pid") {
                let _ = std::fs::remove_file(ent.path());
            } else if name.ends_with(".log") {
                let clean = name.trim_end_matches(".sh.log").to_string();
                let _ = std::fs::rename(ent.path(), failed.join(format!("{clean}.interrupted.{}.log", queue::unix_now())));
                dlog(root, "sweep_stray_log", ev(json!({"from": name})));
            }
        }
    }
    // Recover claim-staging dirs in pending/: a kill -9 between the
    // `.sh` move and the stage->entry rename leaves
    // `.tmp.claim.<pid>.<name>/task.sh` — dot-prefixed, so listings
    // never see it and the task would be silently lost. Restore it
    // to a complete `pending/<name>` dir entry (or back to the flat
    // file when a dir already holds that name); an empty stage is
    // pure litter and is just removed.
    if let Ok(rd) = std::fs::read_dir(&pending) {
        for ent in rd.flatten() {
            let fname = ent.file_name().to_string_lossy().into_owned();
            let Some(rest) = fname.strip_prefix(".tmp.claim.") else {
                continue;
            };
            let Some(orig) = rest.split_once('.').map(|(_, n)| n).filter(|n| !n.is_empty()) else {
                continue;
            };
            let stage = ent.path();
            if stage.join("task.sh").exists() {
                let target = pending.join(orig);
                if target.exists() {
                    let _ = std::fs::rename(stage.join("task.sh"), pending.join(format!("{orig}.sh")));
                    let _ = std::fs::remove_dir_all(&stage);
                } else {
                    let _ = std::fs::rename(&stage, &target);
                }
                dlog(root, "claim_stage_recovered", ev(json!({"task": orig})));
            } else {
                let _ = std::fs::remove_dir_all(&stage);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("shunt-reaptest-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        queue::init(&root).unwrap();
        root
    }

    /// A claim stage stranded by kill -9 (`.sh` already moved into
    /// `.tmp.claim.<pid>.<name>/`) must come back as a complete
    /// pending/<name> dir entry — the task was properly queued and
    /// must not silently vanish behind the dot-prefix.
    #[test]
    fn requeue_recovers_stranded_claim_stage() {
        let root = tmp_root("stage");
        let stage = root.join("pending/.tmp.claim.1234.0100_x");
        std::fs::create_dir_all(&stage).unwrap();
        std::fs::write(stage.join("task.sh"), "echo hi\n").unwrap();
        // plus an empty stage crashed before the .sh move
        std::fs::create_dir_all(root.join("pending/.tmp.claim.1234.0101_y")).unwrap();
        requeue_orphans(&root).unwrap();
        assert!(root.join("pending/0100_x/task.sh").is_file());
        assert_eq!(queue::pending_list(&root).unwrap(), vec!["0100_x".to_string()]);
        let leftovers: Vec<_> = std::fs::read_dir(root.join("pending")).unwrap().flatten().collect();
        assert_eq!(leftovers.len(), 1);
        // ...and the recovered entry claims cleanly.
        assert!(queue::claim(&root, "0100_x").unwrap());
        assert!(root.join("running/0100_x/task.sh").is_file());
        std::fs::remove_dir_all(&root).ok();
    }

    /// When the dir name is already taken, recovery falls back to
    /// the flat file so the task stays listed rather than vanishing.
    #[test]
    fn requeue_recovers_stage_as_flat_on_dir_collision() {
        let root = tmp_root("stagecollide");
        std::fs::create_dir_all(root.join("pending/0100_x")).unwrap();
        std::fs::write(root.join("pending/0100_x/task.sh"), "echo dir\n").unwrap();
        let stage = root.join("pending/.tmp.claim.1234.0100_x");
        std::fs::create_dir_all(&stage).unwrap();
        std::fs::write(stage.join("task.sh"), "echo flat\n").unwrap();
        requeue_orphans(&root).unwrap();
        assert!(root.join("pending/0100_x.sh").is_file());
        assert!(root.join("pending/0100_x/task.sh").is_file());
        std::fs::remove_dir_all(&root).ok();
    }
}
