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
    Ok(())
}
