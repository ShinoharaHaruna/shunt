//! Connection-failure streak counters under `$SHUNT_HOME/state/`.

use super::paths::shunt_home;
use crate::server::fsutil::{private_dir, write_private};
use anyhow::Result;
use std::fs;
use std::path::PathBuf;

/// Consecutive ssh-level connect failures before the escalation hint
/// fires (design.md §4.1 endpoint drift). Agents watch
/// `connect_failures`/`endpoint_stale` in `status --all --json`.
pub const CONN_FAIL_ESCALATE: u32 = 3;

fn conn_fail_path(name: &str) -> Result<PathBuf> {
    Ok(shunt_home()?.join("state").join(format!("{name}.fail")))
}

/// Consecutive ssh connect failures recorded for a client.
pub fn conn_fail_read(name: &str) -> u32 {
    conn_fail_path(name).ok().and_then(|p| fs::read_to_string(p).ok()).and_then(|s| s.trim().parse().ok()).unwrap_or(0)
}

/// ssh-level failure (exit 255): bump the streak, return the count.
pub fn conn_fail_bump(name: &str) -> u32 {
    let n = conn_fail_read(name) + 1;
    if let Ok(p) = conn_fail_path(name) {
        if let Some(d) = p.parent() {
            let _ = private_dir(d);
        }
        let _ = write_private(&p, format!("{n}\n"));
    }
    n
}

/// A successful ssh exec clears the streak.
pub fn conn_fail_reset(name: &str) {
    if let Ok(p) = conn_fail_path(name) {
        if p.exists() {
            let _ = fs::remove_file(p);
        }
    }
}
