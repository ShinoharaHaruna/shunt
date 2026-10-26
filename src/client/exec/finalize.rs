//! Terminal-track moves: quarantine, finalize, and the running-log
//! path (design.md §3.9).

use anyhow::{Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::client::queue;

/// Quarantine: move a stuck entry to `failed/` with a `shunt_error`
/// note — the catch-all exit for entries that can neither run nor be
/// left invisible (claim collisions, failed finalize/requeue
/// renames). A same-name failed/ entry gets a timestamped suffix
/// rather than being clobbered.
pub fn quarantine(root: &Path, track: &str, name: &str, note: &str) -> Result<()> {
    // Nothing to quarantine is a no-op, not an error: when the source
    // entry is already gone (e.g. finalize's rename succeeded and only
    // the exit-file write failed) the real entry sits in done/ — a
    // `failed/<name>/` holding only `shunt_error` would report a task
    // as failed that actually completed.
    let src_dir = root.join(track).join(name);
    if !src_dir.exists() && !root.join(track).join(format!("{name}.sh")).exists() {
        return Ok(());
    }
    let _seq_lock = queue::seq_lock(root)?;
    let failed = root.join("failed");
    std::fs::create_dir_all(&failed)?;
    let mut dest = failed.join(name);
    if dest.exists() || failed.join(format!("{name}.sh")).exists() {
        // A suffix changes the indexed basename; force a live-track bootstrap (§3.3).
        queue::invalidate_dedup_index(root)?;
        dest = failed.join(format!("{name}.quarantined.{}", crate::proto::time::unix_now()));
    }
    if src_dir.is_dir() {
        std::fs::rename(&src_dir, &dest).with_context(|| format!("cannot quarantine {name}"))?;
    } else {
        std::fs::create_dir_all(&dest)?;
        let flat = root.join(track).join(format!("{name}.sh"));
        if flat.exists() {
            std::fs::rename(&flat, dest.join("task.sh")).with_context(|| format!("cannot quarantine {name}"))?;
        }
    }
    let _ = std::fs::write(dest.join("shunt_error"), format!("{note}\n"));
    Ok(())
}

/// Finalize a reaped task: `=== exit` trailer, `exit` file, move the
/// entry dir to done/ (rc 0) or failed/. Legacy flat entries are
/// normalized into the dir layout here, so old queues converge.
/// `timeout_s`: the task exceeded its wall-clock budget and was
/// killed by the daemon — the exit file keeps the observed signal rc
/// (143 TERM / 137 KILL) and a `timeout` marker file records the
/// exceeded limit so agents can distinguish budget death from a
/// plain non-zero rc.
pub fn finalize(root: &Path, base: &str, rc: i32, timeout_s: Option<u64>) -> Result<()> {
    let running = root.join("running");
    // A timed-out task always lands in failed/, even if its last
    // action was a clean exit 0 caught under the TERM.
    let dest_track = root.join(if rc == 0 && timeout_s.is_none() { "done" } else { "failed" });
    std::fs::create_dir_all(&dest_track)?;
    let dest = dest_track.join(base);
    if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(queue::piece_file(root, "running", base, "log")) {
        if let Some(limit) = timeout_s {
            let _ = writeln!(f, "=== timeout: exceeded wall-clock limit {limit}s");
        }
        let _ = writeln!(f, "=== exit {rc} {}", crate::proto::time::rfc3339_offset_now());
    }
    if queue::is_dir_entry(root, "running", base) {
        // Write `exit`/`timeout` INSIDE the entry dir before the
        // rename — not into `dest` after it, as the spec's literal
        // order suggests (design.md §3.9). The sidecars travel with
        // the dir so success is identical, but a write failure now
        // leaves the entry in running/ where the caller's quarantine
        // fallback can still move it to failed/ with a `shunt_error`
        // note. Writing post-rename would let a failed `exit` write
        // strand an exit-less terminal entry: `recent_exits` filters
        // on `exit` existing, so the completed task reports
        // `unknown` forever (status + lazy `task show`).
        let entry = running.join(base);
        std::fs::write(entry.join("exit"), format!("{rc}\n"))?;
        if let Some(limit) = timeout_s {
            let _ = std::fs::write(entry.join("timeout"), format!("{limit}\n"));
        }
        let _ = std::fs::remove_file(entry.join("pid"));
        std::fs::rename(&entry, &dest).with_context(|| format!("cannot move {base} to {}", dest.display()))?;
    } else {
        // Legacy flat entry: no source dir exists to pre-write into,
        // so `exit` lands in `dest` before the pieces move — a failed
        // task.sh rename then leaves dest holding `exit` (+ the
        // caller's quarantine still applies to the flat remainder),
        // never a terminal entry without an exit marker.
        let dest_preexisting = dest.exists();
        std::fs::create_dir_all(&dest)?;
        std::fs::write(dest.join("exit"), format!("{rc}\n"))?;
        if let Some(limit) = timeout_s {
            let _ = std::fs::write(dest.join("timeout"), format!("{limit}\n"));
        }
        if let Err(e) = std::fs::rename(running.join(format!("{base}.sh")), dest.join("task.sh")) {
            // A `dest` we just created holds ONLY `exit` (+`timeout`):
            // left behind it reports the task as done/failed via
            // `list_entries` + `recent_exits` while the caller's
            // quarantine also lands the flat remainder in failed/ —
            // a phantom double-outcome. Remove it, but only when this
            // call created it: a pre-existing `dest` may be a real
            // older same-named entry whose data `remove_dir_all`
            // would destroy.
            if !dest_preexisting {
                let _ = std::fs::remove_dir_all(&dest);
            }
            return Err(e).with_context(|| format!("cannot move {base} to {}", dest.display()));
        }
        let log = running.join(format!("{base}.sh.log"));
        if log.exists() {
            std::fs::rename(&log, dest.join("log"))?;
        }
        let _ = std::fs::remove_file(running.join(format!("{base}.sh.pid")));
    }
    // Terminal tracks are unbounded without a sweep: past the keep
    // cap the oldest entries move to `<track>/.archive/` so `status`
    // scans stay cheap on long-lived queues (design.md §3.9).
    let track_name = if rc == 0 && timeout_s.is_none() { "done" } else { "failed" };
    queue::archive_oldest(root, track_name, queue::track_keep());
    Ok(())
}

/// Exit status → shell-style rc: exit code, or 128+signal.
pub fn status_rc(status: &std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    if let Some(code) = status.code() {
        code
    } else if let Some(sig) = status.signal() {
        128 + sig
    } else {
        1
    }
}

/// Convenience: the path of a running task's log (`running/<name>/log`).
pub fn running_log(root: &Path, base: &str) -> PathBuf {
    queue::piece_file(root, "running", base, "log")
}
