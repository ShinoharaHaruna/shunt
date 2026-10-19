//! Daemon liveness and self-daemonization: pidfile + `/proc/<pid>`
//! discipline (design.md §3.4).

use anyhow::{Context, Result};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::agent::queue;

/// Daemon liveness: pidfile + `/proc/<pid>` + cmdline mentions
/// `queue::DAEMON_IDENTITY`. Never pgrep (design.md §3.4). Stale
/// pidfiles are removed.
pub fn alive(root: &Path) -> Result<Option<u32>> {
    let pidf = root.join("daemon.pid");
    let pid: u32 = match std::fs::read_to_string(&pidf).ok().and_then(|s| s.trim().parse().ok()) {
        Some(p) => p,
        None => return Ok(None),
    };
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok();
    let hit = cmdline
        .map(|c| {
            let s = String::from_utf8_lossy(&c).replace('\0', " ");
            s.contains(queue::DAEMON_IDENTITY)
        })
        .unwrap_or(false);
    if hit {
        Ok(Some(pid))
    } else {
        // stale or foreign pidfile
        let _ = std::fs::remove_file(&pidf);
        Ok(None)
    }
}

/// Total wait for the daemon's pidfile after spawn. A 5 s ceiling
/// misfires on slow disks/container cold starts (the daemon *is*
/// coming up; the caller just reports it dead). Default 15 s,
/// `SHUNT_START_WAIT_S` overrides; still bounded — a wedge in the
/// spawned child must not hang `start` forever.
fn start_wait() -> Duration {
    std::env::var("SHUNT_START_WAIT_S").ok().and_then(|s| s.parse::<u64>().ok()).filter(|&n| n > 0).map(Duration::from_secs).unwrap_or(Duration::from_secs(15))
}

/// Self-daemonize: spawn `shunt daemon --root <root>` detached
/// (process_group(0), stdio → daemon.log) and wait for the pidfile.
/// Idempotent: returns the live pid when already running.
pub fn start(root: &Path) -> Result<u32> {
    if let Some(pid) = alive(root)? {
        return Ok(pid);
    }
    queue::init(root)?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(root.join("daemon.log"))?;
    let err = log.try_clone()?;
    let exe = std::env::current_exe()?;
    let mut child = Command::new(exe);
    child.arg("daemon").arg("--root").arg(root).stdin(Stdio::null()).stdout(log).stderr(err).process_group(0);
    let mut spawned = child.spawn().context("cannot spawn daemon")?;
    drop(spawned.stdin.take());
    // the daemon writes its own pidfile; wait until liveness confirms
    let wait = start_wait();
    for _ in 0..wait.as_millis() / 50 {
        if let Some(pid) = alive(root)? {
            return Ok(pid);
        }
        if let Ok(Some(_)) = spawned.try_wait() {
            anyhow::bail!("daemon exited during startup; check {}", root.join("daemon.log").display());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    anyhow::bail!("daemon did not come up within {} s; check {}", wait.as_secs(), root.join("daemon.log").display())
}

/// Signal the daemon to drain and exit: place the STOP sentinel.
pub fn stop(root: &Path) -> Result<()> {
    std::fs::write(root.join("STOP"), b"")?;
    Ok(())
}
