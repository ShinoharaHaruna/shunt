//! Tunnel state files under `$SHUNT_HOME/tunnels/`: paths, the
//! per-client flock, and lock-holding status reads.

use crate::server::registry;
use anyhow::{Context, Result};
use fs2::FileExt;
use std::fs::File;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::liveness::{client_target, pid_alive};

pub(super) fn tunnels_dir() -> Result<PathBuf> {
    Ok(registry::shunt_home()?.join("tunnels"))
}

pub(super) fn pid_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.pid"))
}

pub(super) fn port_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.port"))
}

#[cfg(test)]
std::thread_local! {
    // Signal actual contention so reader tests never infer it from a sleep.
    pub(super) static ON_LOCK_WAIT: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

// The inode must remain stable: unlinking a lock on release lets a waiter
// and a new caller acquire different inodes for the same client.
pub(super) fn lock_in(dir: &Path, name: &str, timeout: Duration) -> Result<File> {
    registry::valid_name(name)?;
    crate::server::fsutil::private_dir(dir)?;
    let path = dir.join(format!(".{name}.lock"));
    let lock = std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(&path).with_context(|| format!("open tunnel lock {}", path.display()))?;
    lock.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    let deadline = Instant::now() + timeout;
    loop {
        match lock.try_lock_exclusive() {
            Ok(()) => return Ok(lock),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                #[cfg(test)]
                ON_LOCK_WAIT.with(|observer| {
                    if let Some(observer) = observer.borrow_mut().take() {
                        observer();
                    }
                });
                if Instant::now() >= deadline {
                    anyhow::bail!("timed out waiting for tunnel lock {}; retry the tunnel command", path.display());
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => return Err(e).context("lock tunnel state"),
        }
    }
}

pub(super) fn client_lock(dir: &Path, name: &str) -> Result<File> {
    lock_in(dir, name, Duration::from_secs(30))
}

/// Reported tunnel state.
pub struct TunnelState {
    pub alive: bool,
    pub port: Option<u16>,
    /// Pid is dead but state files remain: the box's env may still
    /// carry ALL_PROXY pointing at a dead proxy (poisoned).
    pub stale: bool,
}

pub(super) fn read_pid(dir: &Path, name: &str) -> Option<u32> {
    let s = std::fs::read_to_string(pid_path(dir, name)).ok()?;
    s.trim().parse().ok()
}

/// A tunnel was set up for `name` at some point: a pid or port
/// state file exists, so the box's env may still carry ALL_PROXY
/// even after the ssh process died (endpoint drift, reboot).
pub(super) fn recorded_in(dir: &Path, name: &str) -> bool {
    pid_path(dir, name).exists() || port_path(dir, name).exists()
}

/// `recorded_in` against the real `$SHUNT_HOME/tunnels/`.
pub fn recorded(name: &str) -> Result<bool> {
    let dir = tunnels_dir()?;
    let _lock = client_lock(&dir, name)?;
    Ok(recorded_in(&dir, name))
}

pub(super) fn recorded_port(dir: &Path, name: &str) -> Option<u16> {
    std::fs::read_to_string(port_path(dir, name)).ok().and_then(|s| s.trim().parse::<u16>().ok())
}

/// `target` is the `user@host` needle when the caller knows the
/// client row — pass `None` when the row is unloadable (liveness
/// then falls back to the port needle alone).
pub(super) fn status_in(dir: &Path, name: &str, target: Option<&str>) -> TunnelState {
    let port = recorded_port(dir, name);
    let alive = matches!(read_pid(dir, name), Some(pid) if pid_alive(pid, port, target));
    let stale = !alive && recorded_in(dir, name);
    TunnelState { alive, port, stale }
}

/// Current tunnel state for a client (no network needed).
pub fn status(name: &str) -> Result<TunnelState> {
    // The row gives the `user@host` needle; an unloadable/missing
    // row degrades to port-only matching rather than failing status.
    let target = registry::load_client(name).ok().and_then(|c| client_target(&c));
    let dir = tunnels_dir()?;
    let _lock = client_lock(&dir, name)?;
    Ok(status_in(&dir, name, target.as_deref()))
}

/// List every recorded tunnel name (clients/tunnels dir).
pub fn list() -> Result<Vec<String>> {
    let dir = tunnels_dir()?;
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for e in std::fs::read_dir(&dir)? {
        let e = e?;
        let name = e.file_name().to_string_lossy().into_owned();
        // Either record can survive a partial setup/cleanup; both mean
        // the box may still carry ALL_PROXY and needs self-heal.
        if let Some(n) = name.strip_suffix(".pid").or_else(|| name.strip_suffix(".port")) {
            out.push(n.to_string());
        }
    }
    out.sort();
    out.dedup();
    let mut recorded = Vec::new();
    for name in out {
        let _lock = client_lock(&dir, &name)?;
        if recorded_in(&dir, &name) {
            recorded.push(name);
        }
    }
    Ok(recorded)
}

pub(super) fn remove_state_file(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("remove tunnel state {}; repair the path and retry", path.display())),
    }
}
