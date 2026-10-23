//! Idle poweroff (design.md §3.11): arm on `shutdown_on_idle`, count
//! down `shutdown_grace_s` with per-second cancels, final re-check,
//! last-moment command resolution (`shutdown_cmd` file > `$SHUTDOWN_CMD`
//! env > `/usr/bin/shutdown`).

use anyhow::{Context, Result};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use crate::client::queue::{self, DEFAULT_GRACE_S};

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
    let flag = root.join("shutdown_on_idle");
    match std::fs::remove_file(&flag) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("cannot disarm idle shutdown: cannot remove {}; check the flag path and parent directory permissions, then retry `shunt shutdown off` with the same --root", flag.display())),
    }
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
/// at check time (same discipline as §3.4). This compatibility wrapper
/// logs failures; the daemon uses the checked variant and refuses startup.
pub fn register_qroot(root: &Path) {
    if let Err(error) = try_register_qroot(root) {
        registration_warning(root, &error);
    }
}

pub(crate) fn registration_warning(root: &Path, error: &anyhow::Error) {
    crate::client::daemon::dlog(
        root,
        "qroot_registration_failed",
        crate::client::daemon::ev(serde_json::json!({
            "warning": "daemon cannot safely run without box-level registration; repair the registry path/permissions and restart the daemon",
            "error": format!("{error:#}")
        })),
    );
}

pub(crate) fn try_register_qroot(root: &Path) -> Result<()> {
    let dir = registry_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create qroot registry {}", dir.display()))?;
    let canon = std::fs::canonicalize(root).context("cannot canonicalize qroot")?;
    let path = canon.to_str().context("qroot registry requires a UTF-8 path")?;
    anyhow::ensure!(!path.contains(['\n', '\r']), "qroot registry requires a single-line path");
    std::fs::write(dir.join(registry_key(&canon)), format!("{path}\n")).with_context(|| format!("cannot write qroot registration in {}", dir.display()))
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
/// its registry entry. Scan failures also block: this compatibility API
/// logs a warning and returns the registry path as a conservative blocker.
pub fn busy_peers(root: &Path) -> Vec<PathBuf> {
    let dir = registry_dir();
    let allow_missing = matches!(std::fs::symlink_metadata(root.join("daemon.pid")), Err(e) if e.kind() == std::io::ErrorKind::NotFound);
    match busy_peers_in(&dir, root, allow_missing) {
        Ok(peers) => peers,
        Err(error) => {
            registry_warning(root, &error);
            // Preserve the Vec API: an unknown registry is a blocker, not
            // evidence of idleness. Callers needing peer identities use the
            // checked API instead of this conservative registry-path sentinel.
            vec![dir]
        }
    }
}

pub(crate) fn registry_warning(root: &Path, error: &anyhow::Error) {
    crate::client::daemon::dlog(
        root,
        "idle_shutdown_blocked",
        crate::client::daemon::ev(serde_json::json!({
            "why": "peer registry unavailable",
            "warning": "shutdown remains armed; repair the qroot registry path, permissions, or malformed records before retrying",
            "error": format!("{error:#}")
        })),
    );
}

pub(crate) fn checked_busy_peers(root: &Path) -> Result<Vec<PathBuf>> {
    // A running daemon already registered: disappearance is not a pristine
    // initial registry and cannot prove that the other queues are idle.
    busy_peers_in(&registry_dir(), root, false)
}

fn busy_peers_in(dir: &Path, root: &Path, allow_missing: bool) -> Result<Vec<PathBuf>> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if allow_missing && e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("cannot scan qroot registry {}", dir.display())),
    };
    scan_peer_entries(dir, root, rd.map(|ent| ent.map(|ent| ent.path())))
}

fn scan_peer_entries(dir: &Path, root: &Path, entries: impl Iterator<Item = std::io::Result<PathBuf>>) -> Result<Vec<PathBuf>> {
    let self_key = registry_key(root);
    let mut busy = Vec::new();
    for entry in entries {
        let path = entry.with_context(|| format!("cannot iterate qroot registry {}", dir.display()))?;
        let record = std::fs::read_to_string(&path).with_context(|| format!("cannot read peer record {}", path.display()))?;
        let value = record.strip_suffix('\n').unwrap_or(&record);
        let peer = PathBuf::from(value);
        anyhow::ensure!(peer.is_absolute() && !value.contains(['\n', '\r', '\0']), "malformed peer record {}: expected one absolute qroot path", path.display());
        if path.file_name().is_some_and(|name| name == self_key.as_str()) {
            continue;
        }
        match crate::client::daemon::check_alive(&peer).with_context(|| format!("cannot verify peer {}", peer.display()))? {
            Some(_) if pending_nonempty(&peer) || running_nonempty(&peer) => busy.push(peer),
            Some(_) => {}
            None => {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    Ok(busy)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_registry_scan_fails_closed() {
        let base = std::env::temp_dir().join(format!("shunt-registry-errors-{}", std::process::id()));
        let reg = base.join("qroots");
        let root = base.join("self");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&root).unwrap();
        assert!(busy_peers_in(&reg, &root, true).unwrap().is_empty(), "initial absent registry is safe for legacy callers");
        assert!(busy_peers_in(&reg, &root, false).is_err(), "registered daemon must not accept a vanished registry");
        std::fs::write(&reg, "not a directory").unwrap();
        assert!(busy_peers_in(&reg, &root, true).is_err());
        std::fs::remove_file(&reg).unwrap();
        std::fs::create_dir(&reg).unwrap();
        let failure = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "injected directory iteration failure");
        let error = scan_peer_entries(&reg, &root, std::iter::once(Err(failure))).unwrap_err();
        assert!(format!("{error:#}").contains("cannot iterate qroot registry"));
        let entry = reg.join("peer");
        for record in [b"".as_slice(), b"relative/path\n", b"/absolute\n/another\n", b"/absolute\0", b"\xff"] {
            std::fs::write(&entry, record).unwrap();
            assert!(busy_peers_in(&reg, &root, false).is_err(), "invalid record: {record:?}");
            assert!(entry.exists(), "unknown records must not be swept");
        }
        std::fs::remove_file(&entry).unwrap();
        std::fs::create_dir(&entry).unwrap();
        assert!(busy_peers_in(&reg, &root, false).is_err(), "unreadable entry cannot be skipped");
        std::fs::remove_dir(&entry).unwrap();
        let peer = base.join("peer");
        std::fs::create_dir_all(&peer).unwrap();
        std::fs::write(&entry, format!("{}\n", peer.display())).unwrap();
        let pidfile = peer.join("daemon.pid");
        std::fs::create_dir(&pidfile).unwrap();
        assert!(busy_peers_in(&reg, &root, false).is_err(), "unreadable peer pidfile cannot prove death");
        std::fs::remove_dir(&pidfile).unwrap();
        std::fs::write(&pidfile, "garbage").unwrap();
        assert!(busy_peers_in(&reg, &root, false).is_err(), "malformed pidfile cannot prove death");
        std::fs::remove_file(&pidfile).unwrap();
        assert!(busy_peers_in(&reg, &root, false).unwrap().is_empty(), "missing pidfile is legitimately stale");
        assert!(!entry.exists());
        std::fs::remove_dir_all(&base).unwrap();
    }

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

        let busy = busy_peers_in(&reg, &self_root, false).unwrap();
        assert!(busy.is_empty());
        assert!(pidf.exists(), "busy_peers must not delete the peer's daemon.pid");
        assert!(reg.read_dir().unwrap().next().is_none(), "proven-dead peer entry should be swept");
        std::fs::remove_dir_all(&base).ok();
    }
}
