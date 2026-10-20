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

/// True when pending/ holds at least one entry (dir or `.sh` file)
/// OR when it cannot be proven empty: a read error means a real task
/// may sit there unseen, so poweroff judgments fail closed — same
/// direction as Probe::Unknown ⇒ alive for liveness (§3.4/§3.11).
pub fn pending_nonempty(root: &Path) -> bool {
    queue::pending_list(root).map(|v| !v.is_empty()).unwrap_or(true)
}

/// Same fail-closed judgment for running/ (busy_peers only): an
/// unreadable running/ must block a box-level poweroff, not clear it.
fn running_nonempty(root: &Path) -> bool {
    queue::running_list(root).map(|v| !v.is_empty()).unwrap_or(true)
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
/// swept as they are found. Liveness here is strictly read-only
/// (`check_alive`): the peer daemon owns its `daemon.pid`, and an
/// unverifiable-but-alive pid must not cost the peer its pidfile or
/// its registry entry.
pub fn busy_peers(root: &Path) -> Vec<PathBuf> {
    busy_peers_in(&registry_dir(), root)
}

/// `busy_peers` over an explicit registry dir (test hook: avoids
/// mutating process-global `SHUNT_STATE`).
fn busy_peers_in(dir: &Path, root: &Path) -> Vec<PathBuf> {
    let self_key = registry_key(root);
    let mut busy = Vec::new();
    let rd = match std::fs::read_dir(dir) {
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
        match crate::agent::daemon::check_alive(&peer) {
            // Queue scans fail closed (§3.11): a live sibling whose
            // pending/ or running/ cannot be read still counts as
            // busy — cannot prove empty ⇒ do not power off under it.
            Ok(Some(_)) if pending_nonempty(&peer) || running_nonempty(&peer) => {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Queue-emptiness fails closed (§3.11): `pending`/`running` that
    /// exist but cannot be listed (here: a file where the dir should
    /// be → ENOTDIR) report non-empty, so an unreadable queue can
    /// never arm the poweroff countdown or clear a busy peer.
    #[test]
    fn queue_nonempty_fails_closed_on_read_error() {
        let root = std::env::temp_dir().join(format!("shunt-failclosed-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("pending"), b"not a dir").unwrap();
        std::fs::write(root.join("running"), b"not a dir").unwrap();
        assert!(pending_nonempty(&root), "unreadable pending/ must count as non-empty");
        assert!(running_nonempty(&root), "unreadable running/ must count as non-empty");
        // Control: a genuinely absent/empty pending/ still reads empty.
        std::fs::remove_file(root.join("pending")).unwrap();
        assert!(!pending_nonempty(&root));
        std::fs::create_dir_all(root.join("pending")).unwrap();
        assert!(!pending_nonempty(&root));
        std::fs::remove_dir_all(&root).ok();
    }

    /// `shutdown on` with no grace argument arms at the default
    /// (design.md §3.11: ''|0 → 60); a previous explicit grace must
    /// not survive a re-arm.
    #[test]
    fn arm_without_grace_uses_default() {
        let root = std::env::temp_dir().join(format!("shunt-grace-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        arm(&root, Some(30)).unwrap();
        assert_eq!(grace(&root), 30);
        arm(&root, None).unwrap();
        let st = status(&root).unwrap();
        assert!(st.armed);
        assert_eq!(st.grace_s, DEFAULT_GRACE_S);
        assert_eq!(grace(&root), 60);
        std::fs::remove_dir_all(&root).ok();
    }

    /// `busy_peers` is strictly read-only on the peer's qroot: a peer
    /// whose `daemon.pid` names an alive-but-foreign pid (recycled —
    /// our test binary's cmdline cannot match `daemon` at argv[1]) is
    /// proven dead, so its registry entry is swept, but the pidfile
    /// itself is NEVER removed — the peer daemon owns it.
    #[test]
    fn busy_peers_never_removes_peer_pidfile() {
        let base = std::env::temp_dir().join(format!("shunt-peers-test-{}", std::process::id()));
        let reg = base.join("qroots");
        let self_root = base.join("self-q");
        let peer = base.join("peer-q");
        std::fs::create_dir_all(&reg).unwrap();
        std::fs::create_dir_all(&self_root).unwrap();
        std::fs::create_dir_all(peer.join("pending")).unwrap();
        let pidf = peer.join("daemon.pid");
        std::fs::write(&pidf, format!("{}", std::process::id())).unwrap();
        // pending work so a wrong "alive" verdict would show up in `busy`
        std::fs::create_dir_all(peer.join("pending/t1")).unwrap();
        let canon = std::fs::canonicalize(&peer).unwrap();
        std::fs::write(reg.join(registry_key(&canon)), format!("{}\n", canon.display())).unwrap();

        let busy = busy_peers_in(&reg, &self_root);
        assert!(busy.is_empty());
        assert!(pidf.exists(), "busy_peers must not delete the peer's daemon.pid");
        assert!(reg.read_dir().unwrap().next().is_none(), "proven-dead peer entry should be swept");
        std::fs::remove_dir_all(&base).ok();
    }
}
