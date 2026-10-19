//! Daemon liveness and self-daemonization: pidfile + `/proc/<pid>`
//! discipline (design.md §3.4).

use anyhow::{Context, Result};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::agent::queue;

/// NUL-separated `/proc/<pid>/cmdline` → this is our daemon when
/// argv[0]'s basename equals this binary's basename AND some argv
/// element is exactly `queue::DAEMON_IDENTITY` (the subcommand).
/// Basename + whole-element beats a substring test: a binary
/// installed as `shuntd`/`shunt-1.2` still matches itself, while an
/// unrelated `foo-shunt daemon` cannot false-match (design.md §3.4).
fn is_daemon_cmdline(cmdline: &[u8], exe_name: &str) -> bool {
    let mut argv = cmdline.split(|&b| b == 0).filter(|a| !a.is_empty());
    let Some(argv0) = argv.next() else {
        return false;
    };
    let base = argv0.rsplit(|&b| b == b'/').next().unwrap_or(argv0);
    if base != exe_name.as_bytes() {
        return false;
    }
    argv.any(|a| a == queue::DAEMON_IDENTITY.as_bytes())
}

/// Daemon liveness: pidfile + `/proc/<pid>` + cmdline identity check
/// (`is_daemon_cmdline`). Never pgrep (design.md §3.4). Stale
/// pidfiles are removed.
pub fn alive(root: &Path) -> Result<Option<u32>> {
    let pidf = root.join("daemon.pid");
    let pid: u32 = match std::fs::read_to_string(&pidf).ok().and_then(|s| s.trim().parse().ok()) {
        Some(p) => p,
        None => return Ok(None),
    };
    let exe_name = std::env::current_exe().ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));
    let hit = exe_name.as_deref().is_some_and(|n| std::fs::read(format!("/proc/{pid}/cmdline")).ok().is_some_and(|c| is_daemon_cmdline(&c, n)));
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

#[cfg(test)]
mod tests {
    use super::is_daemon_cmdline;

    fn argv(parts: &[&str]) -> Vec<u8> {
        let mut v = Vec::new();
        for p in parts {
            v.extend_from_slice(p.as_bytes());
            v.push(0);
        }
        v
    }

    /// Identity = argv[0] basename == our exe basename + a whole
    /// `daemon` argv element (design.md §3.4): renamed binaries
    /// self-match, unrelated `foo-shunt daemon` does not.
    #[test]
    fn cmdline_identity_basename_plus_element() {
        assert!(is_daemon_cmdline(&argv(&["/usr/bin/shunt", "daemon", "--root", "/q"]), "shunt"));
        // same binary installed under another basename
        assert!(is_daemon_cmdline(&argv(&["shuntd", "daemon", "--root", "/q"]), "shuntd"));
        // basename mismatch: foreign binary must not match
        assert!(!is_daemon_cmdline(&argv(&["/usr/bin/foo-shunt", "daemon"]), "shunt"));
        // `daemon` only as a substring of an argv element
        assert!(!is_daemon_cmdline(&argv(&["shunt", "daemonesque"]), "shunt"));
        assert!(!is_daemon_cmdline(&argv(&["shunt", "--root", "/q"]), "shunt"));
        assert!(!is_daemon_cmdline(&[], "shunt"));
    }
}
