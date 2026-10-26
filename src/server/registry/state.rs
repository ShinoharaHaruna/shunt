//! Connection-failure streak counters under `$SHUNT_HOME/state/`.

use super::paths::shunt_home;
use crate::server::fsutil::{private_dir, write_private};
use anyhow::Result;
use fs2::FileExt;
use std::fs;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;

/// Consecutive ssh-level connect failures before the escalation hint
/// fires (design.md §4.1 endpoint drift). Agents watch
/// `connect_failures`/`endpoint_stale` in `status --all --json`.
pub const CONN_FAIL_ESCALATE: u32 = 3;

fn conn_fail_path(name: &str) -> Result<PathBuf> {
    Ok(shunt_home()?.join("state").join(format!("{name}.fail")))
}

/// Sidecar flock serializing the `.fail` read-modify-write / unlink
/// (same pattern as `.seq.lock` and `daemon.lock`). The lock cannot
/// live on `<name>.fail` itself: `conn_fail_reset` unlinks that file,
/// so a waiter would hold a stale inode while another process creates
/// and locks a fresh one. Best-effort — a lock failure degrades to
/// the previous unlocked behavior rather than breaking the counter.
fn conn_fail_lock(name: &str) -> Option<fs::File> {
    let lp = shunt_home().ok()?.join("state").join(format!("{name}.fail.lock"));
    if let Some(d) = lp.parent() {
        private_dir(d).ok()?;
    }
    let f = fs::OpenOptions::new().create(true).append(true).mode(0o600).open(lp).ok()?;
    f.lock_exclusive().ok()?;
    Some(f)
}

/// The sidecar is `<count>\n<epoch-since-last-failure>\n`. The first
/// line stays the plain count so a file this build did not write
/// (the pre-timestamp single-line format) still reads as a streak —
/// the missing epoch only means "no penalty clock", never a lost
/// count.
fn read_fail_file(name: &str) -> (u32, Option<i64>) {
    let Some(raw) = conn_fail_path(name).ok().and_then(|p| fs::read_to_string(p).ok()) else {
        return (0, None);
    };
    let mut lines = raw.lines();
    let count = lines.next().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    let at = lines.next().and_then(|s| s.trim().parse().ok());
    (count, at)
}

/// Consecutive ssh connect failures recorded for a client.
pub fn conn_fail_read(name: &str) -> u32 {
    read_fail_file(name).0
}

/// Wall-clock (epoch seconds) of the newest recorded connect failure.
/// `None` for a clean client or a streak file without the timestamp —
/// the probe scheduler then treats the next attempt as due.
pub fn conn_fail_at(name: &str) -> Option<i64> {
    read_fail_file(name).1
}

/// ssh-level failure (exit 255): bump the streak, return the count.
pub fn conn_fail_bump(name: &str) -> u32 {
    // Concurrent processes (a dispatch sweep plus `status`/`client
    // exec`) would interleave read→write and lose a bump.
    let _lock = conn_fail_lock(name);
    let n = conn_fail_read(name) + 1;
    if let Ok(p) = conn_fail_path(name) {
        if let Some(d) = p.parent() {
            let _ = private_dir(d);
        }
        let _ = write_private(&p, format!("{n}\n{}\n", crate::proto::time::unix_now()));
    }
    n
}

/// A successful ssh exec clears the streak — and re-tightens the
/// per-client known_hosts, which `StrictHostKeyChecking=accept-new`
/// may have just created at umask permissions.
pub fn conn_fail_reset(name: &str) {
    // Same sidecar lock as the bump: without it a concurrent bump's
    // write could recreate the file this unlink just removed.
    let _lock = conn_fail_lock(name);
    if let Ok(p) = conn_fail_path(name) {
        if p.exists() {
            let _ = fs::remove_file(p);
        }
    }
    if let Ok(kh) = super::paths::known_hosts_path(name) {
        if kh.exists() {
            let _ = fs::set_permissions(&kh, fs::Permissions::from_mode(0o600));
        }
    }
}
