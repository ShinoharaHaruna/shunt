//! Connection-failure streak counters under `$SHUNT_HOME/state/`.

use super::paths::shunt_home;
use super::ClientEntry;
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
/// Automatic dispatch and background probes quarantine at this streak.
pub const CONN_FAIL_QUARANTINE: u32 = 9;

fn conn_fail_path(name: &str) -> Result<PathBuf> {
    Ok(shunt_home()?.join("state").join(format!("{name}.fail")))
}

fn is_quarantined(name: &str, failures: u32) -> bool {
    super::io::load_client_entry(name).ok().is_some_and(|client| matches!(super::viability::inactive_reason(&client, failures), Some(super::viability::InactiveReason::Quarantined { .. })))
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

/// The client row's own file — the registry's source of truth for a box,
/// so its mtime doubles as "a human last touched this row".
fn client_row_path(name: &str) -> Option<PathBuf> {
    Some(shunt_home().ok()?.join("clients").join(format!("{name}.toml")))
}

/// The streak to act on: the recorded count, unless the row file is
/// newer than the newest recorded failure — then the streak is stale and
/// is cleared first.
///
/// The case this exists for is a renewed subscription. `expire` is a
/// hand-edited row field (§9), and the give-up pause would otherwise
/// deadlock: a streak only clears on a successful ssh, while a paused
/// client is never ssh'd. Renewal moves the deadline with one edit, so
/// the edit IS the retry signal — the same way `client update --host`
/// clears the streak for a new endpoint. Any other row edit (a new
/// qroot, a fixed env_setup) earns the same single probe; a box that is
/// still gone simply re-bumps.
pub fn conn_fail_streak(client: &ClientEntry) -> u32 {
    let count = conn_fail_read(&client.name);
    if count == 0 {
        return 0;
    }
    let (Some(row), Ok(fail)) = (client_row_path(&client.name), conn_fail_path(&client.name)) else {
        return count;
    };
    let mtime = |p: &std::path::Path| fs::metadata(p).and_then(|m| m.modified());
    let (Ok(row_at), Ok(fail_at)) = (mtime(&row), mtime(&fail)) else {
        return count;
    };
    if row_at <= fail_at {
        return count;
    }
    conn_fail_reset(&client.name);
    0
}

/// ssh-level failure (exit 255): bump the streak, return the count.
pub fn conn_fail_bump(name: &str) -> u32 {
    // Concurrent processes (a dispatch sweep plus `status`/`client
    // exec`) would interleave read→write and lose a bump.
    let _lock = conn_fail_lock(name);
    let n = conn_fail_read(name) + 1;
    let written = conn_fail_path(name).ok().is_some_and(|p| {
        if let Some(d) = p.parent() {
            let _ = private_dir(d);
        }
        write_private(&p, format!("{n}\n{}\n", crate::proto::time::unix_now())).is_ok()
    });
    if written && n == CONN_FAIL_QUARANTINE && is_quarantined(name, n) {
        eprintln!("client {name} quarantined after {n} consecutive connect failures — edit the row or use `shunt client update` to retry");
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
    let failures = conn_fail_read(name);
    let was_quarantined = failures >= CONN_FAIL_QUARANTINE && is_quarantined(name, failures);
    let cleared = conn_fail_path(name).ok().is_some_and(|p| !p.exists() || fs::remove_file(p).is_ok());
    if cleared && was_quarantined {
        eprintln!("client {name} quarantine cleared — automatic probes resumed");
    }
    if let Ok(kh) = super::paths::known_hosts_path(name) {
        if kh.exists() {
            let _ = fs::set_permissions(&kh, fs::Permissions::from_mode(0o600));
        }
    }
}
