//! Idle poweroff (design.md §3.11): arm on `shutdown_on_idle`, count
//! down `shutdown_grace_s` with per-second cancels, final re-check,
//! last-moment command resolution (`shutdown_cmd` file > `$SHUTDOWN_CMD`
//! env > `/usr/bin/shutdown`).

use anyhow::Result;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use crate::agent::queue::{self, DEFAULT_GRACE_S};

/// Write `<qroot>/shutdown_cmd` — the poweroff command the daemon
/// execs at countdown end (design.md §3.11 resolution chain).
pub fn set_cmd(root: &Path, cmd: &str) -> Result<()> {
    std::fs::write(root.join("shutdown_cmd"), format!("{cmd}\n"))?;
    Ok(())
}

/// Arm idle-shutdown, optionally overriding the grace file.
pub fn arm(root: &Path, grace_s: Option<u64>) -> Result<()> {
    std::fs::write(root.join("shutdown_on_idle"), b"")?;
    if let Some(g) = grace_s {
        std::fs::write(root.join("shutdown_grace_s"), format!("{g}\n"))?;
    } else {
        // No override given: clear any grace file left by a previous
        // `shutdown on <n>` so the default applies — resolution is
        // "file > default", and a stale file must not survive re-arm.
        let _ = std::fs::remove_file(root.join("shutdown_grace_s"));
    }
    Ok(())
}

/// Disarm: remove the flag. A countdown in progress cancels within 1 s.
pub fn disarm(root: &Path) -> Result<()> {
    let _ = std::fs::remove_file(root.join("shutdown_on_idle"));
    Ok(())
}

/// Grace seconds: file > default 60 (`''|0` → default, as the bash).
pub fn grace(root: &Path) -> u64 {
    let g = std::fs::read_to_string(root.join("shutdown_grace_s"))
        .ok()
        .and_then(|s| {
            let d: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
            d.parse::<u64>().ok()
        })
        .unwrap_or(0);
    if g == 0 {
        DEFAULT_GRACE_S
    } else {
        g
    }
}

/// Armed state + grace + the command that would fire.
pub fn status(root: &Path) -> Result<crate::proto::wire::ShutdownState> {
    Ok(crate::proto::wire::ShutdownState { armed: root.join("shutdown_on_idle").exists(), grace_s: grace(root), cmd: resolve_cmd(root) })
}

/// Resolve the poweroff command at the last moment: `shutdown_cmd`
/// file > `$SHUTDOWN_CMD` env > `/usr/bin/shutdown`. Last-moment
/// resolution is what lets a test redirect to a marker script.
pub fn resolve_cmd(root: &Path) -> String {
    if let Ok(c) = std::fs::read_to_string(root.join("shutdown_cmd")) {
        let c = c.trim();
        if !c.is_empty() {
            return c.to_string();
        }
    }
    std::env::var("SHUTDOWN_CMD").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/usr/bin/shutdown".to_string())
}

/// True when pending/ holds at least one entry (dir or `.sh` file).
pub fn pending_nonempty(root: &Path) -> bool {
    queue::pending_list(root).map(|v| !v.is_empty()).unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Box-level qroot registry (design.md §3.11): several daemons may run
// different qroots on one box; idle-shutdown must not power off while a
// sibling daemon still has work. Each daemon registers its qroot here at
// start, deregisters on every exit path, and the fire path checks peers.
// ---------------------------------------------------------------------------

/// Registry dir: `$SHUNT_STATE/qroots` > `$XDG_STATE_HOME/shunt/qroots` >
/// `~/.local/state/shunt/qroots` > `/tmp/shunt-$USER/qroots`. Box-level
/// by construction — it must not live under any single qroot.
pub fn registry_dir() -> PathBuf {
    if let Ok(d) = std::env::var("SHUNT_STATE") {
        return PathBuf::from(d).join("qroots");
    }
    if let Ok(d) = std::env::var("XDG_STATE_HOME") {
        return PathBuf::from(d).join("shunt").join("qroots");
    }
    if let Ok(h) = std::env::var("HOME") {
        return PathBuf::from(h).join(".local/state/shunt/qroots");
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".into());
    PathBuf::from(format!("/tmp/shunt-{user}/qroots"))
}

/// Registry file name for a qroot: a stable hash of the canonical path.
fn registry_key(root: &Path) -> String {
    let canon = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut h = DefaultHasher::new();
    canon.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Register this daemon's qroot in the box-level registry. Content is
/// the canonical qroot path; liveness is read from `<qroot>/daemon.pid`
/// at check time (same discipline as §3.4).
pub fn register_qroot(root: &Path) {
    let dir = registry_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let canon = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let _ = std::fs::write(dir.join(registry_key(&canon)), format!("{}\n", canon.display()));
}

/// Remove this daemon's registry entry (every exit path calls this).
pub fn deregister_qroot(root: &Path) {
    let _ = std::fs::remove_file(registry_dir().join(registry_key(root)));
}

/// Sibling daemons that still have work: registered qroots whose daemon
/// is live (`daemon.pid` + cmdline check) AND whose queue holds pending
/// or running entries. Stale entries (dead or vanished daemon) are
/// swept as they are found.
pub fn busy_peers(root: &Path) -> Vec<PathBuf> {
    let dir = registry_dir();
    let self_key = registry_key(root);
    let mut busy = Vec::new();
    let rd = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(_) => return busy,
    };
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().into_owned();
        if name == self_key {
            continue;
        }
        let peer: PathBuf = match std::fs::read_to_string(ent.path()) {
            Ok(s) => PathBuf::from(s.trim()),
            Err(_) => continue,
        };
        match crate::agent::daemon::alive(&peer) {
            Ok(Some(_)) if pending_nonempty(&peer) || queue::running_list(&peer).map(|v| !v.is_empty()).unwrap_or(false) => {
                busy.push(peer);
            }
            Ok(Some(_)) => {}
            // dead daemon or unverifiable qroot: sweep the entry
            _ => {
                let _ = std::fs::remove_file(ent.path());
            }
        }
    }
    busy
}
