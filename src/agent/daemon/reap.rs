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
            // Two live-group proofs (the §3.9a gate, same as the
            // signal path): a start_time-verified live leader, OR a
            // dead leader while the group still holds members — a
            // live pgid can't be recycled, so that remnant is
            // provably this task's own (detached grandchildren from
            // `cmd &` survive the leader). A live pid at pgid with a
            // mismatched start_time is a recycled stranger's group
            // and stays silent.
            if exec::pgid_is_task_group(pgid, start) {
                dlog(
                    root,
                    "orphan_live_pgid",
                    ev(json!({"task": base, "pgid": pgid,
                           "leader_dead": exec::proc_pgrp_start(pgid).is_none(),
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
            // Enqueue staging dirs (`.tmp.<pid>.<n>/`, queue.rs
            // enqueue) stranded by kill -9 are DELETED, not
            // recovered: an enqueue that never completed its rename
            // was never acked, so the submitter believes the submit
            // failed (the server respools it) — materializing it
            // here would run a task nobody expects and risk a
            // duplicate when the respush lands. Claim stages are
            // different: the task was already queued and claimed,
            // so losing it would be a silent drop.
            if let Some(rest) = fname.strip_prefix(".tmp.") {
                if !rest.starts_with("claim.") {
                    let mut parts = rest.split('.');
                    let looks_like_enqueue_stage = matches!((parts.next(), parts.next(), parts.next()), (Some(a), Some(b), None) if [a, b].iter().all(|p| p.chars().all(|c| c.is_ascii_digit())));
                    if looks_like_enqueue_stage {
                        let _ = std::fs::remove_dir_all(ent.path());
                        dlog(root, "enqueue_stage_swept", ev(json!({"dir": fname})));
                    }
                    continue;
                }
            }
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

    /// An enqueue staging dir (`.tmp.<pid>.<n>/`) stranded by a
    /// kill -9 mid-enqueue is litter: the submit was never acked, so
    /// the submitter believes it failed and the spool will respush.
    /// The sweep must DELETE it — recovering it to pending/ would
    /// resurrect an unexpected task and risk a duplicate run.
    #[test]
    fn requeue_deletes_stranded_enqueue_stage() {
        let root = tmp_root("enqstage");
        let stage = root.join("pending/.tmp.4242.0007");
        std::fs::create_dir_all(&stage).unwrap();
        std::fs::write(stage.join("task.sh"), "echo hi\n").unwrap();
        // A non-numeric `.tmp.*` name is not an enqueue stage — leave it.
        std::fs::create_dir_all(root.join("pending/.tmp.abc")).unwrap();
        requeue_orphans(&root).unwrap();
        assert!(!stage.exists(), "un-acked enqueue stage must be deleted, not recovered");
        assert!(root.join("pending/.tmp.abc").exists(), "non-enqueue .tmp.* litter is out of scope");
        assert!(queue::pending_list(&root).unwrap().is_empty());
        let log = std::fs::read_to_string(root.join("daemon.log")).unwrap();
        assert!(log.contains("enqueue_stage_swept"), "sweep must be loud: {log}");
        std::fs::remove_dir_all(&root).ok();
    }

    /// A dead leader with a live remnant group (the `cmd &` pattern)
    /// must still log `orphan_live_pgid` — a live pgid can't be
    /// recycled, so the group is provably the task's own (§3.9a).
    /// Before the fix the start_time-match gate silenced this case.
    #[test]
    fn requeue_warns_on_leader_dead_remnant_group() {
        use std::os::unix::process::CommandExt;
        let root = tmp_root("remnant");
        let entry = root.join("running/0100_x");
        std::fs::create_dir_all(&entry).unwrap();
        std::fs::write(entry.join("task.sh"), "echo hi\n").unwrap();
        // Leader spawns a sleeper in its own group and exits —
        // proc_pgrp_start(pgid) is None while pgid_alive(pgid) holds.
        let mut child = std::process::Command::new("bash").args(["-c", "(trap '' TERM; exec sleep 300) & exit 0"]).process_group(0).spawn().unwrap();
        let pgid = child.id();
        let _ = child.wait();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !exec::pgid_alive(pgid) {
            assert!(std::time::Instant::now() < deadline, "remnant group never came up");
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        std::fs::write(entry.join("pid"), format!("{pgid}\n")).unwrap();
        requeue_orphans(&root).unwrap();
        let log = std::fs::read_to_string(root.join("daemon.log")).unwrap();
        assert!(log.contains("orphan_live_pgid"), "remnant group must warn: {log}");
        assert!(log.contains("\"leader_dead\":true"), "warning must flag the dead leader: {log}");
        assert!(root.join("pending/0000_requeued_0100_x").is_dir());
        // Clean up the remnant so it can't outlive the test.
        unsafe { libc::kill(-(pgid as i32), libc::SIGKILL) };
        std::fs::remove_dir_all(&root).ok();
    }
}
