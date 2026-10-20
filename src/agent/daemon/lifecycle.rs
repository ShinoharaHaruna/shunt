//! Daemon liveness and self-daemonization: pidfile + `/proc/<pid>`
//! discipline (design.md §3.4).

use anyhow::{Context, Result};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::agent::queue;

/// NUL-separated `/proc/<pid>/cmdline` → this is our daemon when
/// argv[0]'s basename equals this binary's basename AND argv[1] —
/// the subcommand slot; this CLI has no pre-subcommand global flags —
/// is exactly `queue::DAEMON_IDENTITY`. Anchoring at argv[1] keeps a
/// recycled pid running `shunt logs daemon`/`kill daemon` (pattern
/// positional) from false-identifying as the daemon. Basename +
/// whole-element beats a substring test: a binary installed as
/// `shuntd`/`shunt-1.2` still matches itself, while an unrelated
/// `foo-shunt daemon` cannot false-match (design.md §3.4).
fn is_daemon_cmdline(cmdline: &[u8], exe_name: &str) -> bool {
    let mut argv = cmdline.split(|&b| b == 0).filter(|a| !a.is_empty());
    let Some(argv0) = argv.next() else {
        return false;
    };
    let base = argv0.rsplit(|&b| b == b'/').next().unwrap_or(argv0);
    if base != exe_name.as_bytes() {
        return false;
    }
    argv.next().is_some_and(|a| a == queue::DAEMON_IDENTITY.as_bytes())
}

/// Probe outcome for one pid (design.md §3.4). `Dead` is PROOF of
/// death only: the process is gone (`/proc/<pid>` absent) or its
/// cmdline was read and does not match (pid recycled — our daemon is
/// provably gone). A cmdline READ FAILURE (EACCES on a different-uid
/// process, transient proc pressure) is `Unknown` — inconclusive, so
/// the fail-safe is to report the daemon alive and touch nothing.
#[derive(Debug, PartialEq, Eq)]
enum Probe {
    Dead,
    Live,
    Unknown,
}

fn probe_pid(pid: u32, exe_name: &str, read_cmdline: impl Fn(u32) -> std::io::Result<Vec<u8>>) -> Probe {
    if !Path::new(&format!("/proc/{pid}")).exists() {
        return Probe::Dead;
    }
    match read_cmdline(pid) {
        Ok(c) if is_daemon_cmdline(&c, exe_name) => Probe::Live,
        Ok(_) => Probe::Dead, // read succeeded, identity mismatch: pid recycled
        Err(_) => Probe::Unknown,
    }
}

/// Shared liveness core. `evict_stale`: remove `daemon.pid` on PROOF
/// of death (`alive`, which owns its own qroot) vs never touch the
/// file (`check_alive`, used on peers — the peer daemon owns its
/// pidfile). `Unknown` never removes and reports alive: fail-safe
/// against double-spawn and against deleting live state.
fn alive_impl(root: &Path, evict_stale: bool, read_cmdline: impl Fn(u32) -> std::io::Result<Vec<u8>>) -> Result<Option<u32>> {
    let pidf = root.join("daemon.pid");
    let pid: u32 = match std::fs::read_to_string(&pidf).ok().and_then(|s| s.trim().parse().ok()) {
        Some(p) => p,
        None => return Ok(None),
    };
    let Some(exe_name) = std::env::current_exe().ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())) else {
        // cannot determine own identity → cannot prove anything
        return Ok(Some(pid));
    };
    match probe_pid(pid, &exe_name, read_cmdline) {
        Probe::Live | Probe::Unknown => Ok(Some(pid)),
        Probe::Dead => {
            if evict_stale {
                // TOCTOU: `run()` writes its pidfile after taking
                // daemon.lock while alive() callers hold no lock, so a
                // new daemon's live pidfile can land between our read
                // and this remove. Re-read and remove only when the
                // content is still the pid we proved dead; a changed
                // pidfile is owned by a live daemon (design.md §3.4).
                let still = std::fs::read_to_string(&pidf).ok().and_then(|s| s.trim().parse::<u32>().ok()) == Some(pid);
                if still {
                    let _ = std::fs::remove_file(&pidf);
                }
            }
            Ok(None)
        }
    }
}

/// Daemon liveness: pidfile + `/proc/<pid>` + cmdline identity check
/// (`is_daemon_cmdline`). Never pgrep (design.md §3.4). Removes the
/// pidfile only on proof of death; a cmdline read failure reports the
/// daemon alive and leaves the file.
pub fn alive(root: &Path) -> Result<Option<u32>> {
    alive_impl(root, true, |pid| std::fs::read(format!("/proc/{pid}/cmdline")))
}

/// Read-only liveness for sibling qroots (`busy_peers`): identical
/// probe, but never removes `daemon.pid` — the peer daemon owns it.
pub fn check_alive(root: &Path) -> Result<Option<u32>> {
    alive_impl(root, false, |pid| std::fs::read(format!("/proc/{pid}/cmdline")))
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
            // Under a start race the loser exits early (daemon.lock
            // held) while the winner's pidfile may land just after
            // our alive() probe above — re-check before reporting a
            // failure; the surviving daemon's pid is the right answer.
            if let Some(pid) = alive(root)? {
                return Ok(pid);
            }
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
    use super::{alive_impl, is_daemon_cmdline, Probe};

    fn argv(parts: &[&str]) -> Vec<u8> {
        let mut v = Vec::new();
        for p in parts {
            v.extend_from_slice(p.as_bytes());
            v.push(0);
        }
        v
    }

    /// Identity = argv[0] basename == our exe basename + argv[1] ==
    /// `daemon` (design.md §3.4): renamed binaries self-match,
    /// unrelated `foo-shunt daemon` does not, and `daemon` at any
    /// other argv position (pattern positional of `logs`/`kill`/
    /// `cancel`) must not match.
    #[test]
    fn cmdline_identity_basename_plus_element() {
        assert!(is_daemon_cmdline(&argv(&["/usr/bin/shunt", "daemon", "--root", "/q"]), "shunt"));
        assert!(is_daemon_cmdline(&argv(&["shunt", "daemon", "--root", "/q"]), "shunt"));
        // same binary installed under another basename
        assert!(is_daemon_cmdline(&argv(&["shuntd", "daemon", "--root", "/q"]), "shuntd"));
        // basename mismatch: foreign binary must not match
        assert!(!is_daemon_cmdline(&argv(&["/usr/bin/foo-shunt", "daemon"]), "shunt"));
        // `daemon` at a later argv position is a pattern, not the subcommand
        assert!(!is_daemon_cmdline(&argv(&["shunt", "logs", "daemon"]), "shunt"));
        assert!(!is_daemon_cmdline(&argv(&["shunt", "kill", "daemon"]), "shunt"));
        // `daemon` only as a substring of an argv element
        assert!(!is_daemon_cmdline(&argv(&["shunt", "daemonesque"]), "shunt"));
        assert!(!is_daemon_cmdline(&argv(&["shunt", "--root", "/q"]), "shunt"));
        assert!(!is_daemon_cmdline(&[], "shunt"));
    }

    /// A live pid whose cmdline cannot be read (EACCES, transient
    /// proc pressure) is inconclusive: `alive` reports the daemon up
    /// and must NOT remove its pidfile — deleting live state is how
    /// duplicate daemons get spawned.
    #[test]
    fn unreadable_cmdline_keeps_pidfile() {
        let root = std::env::temp_dir().join(format!("shunt-alive-test-{}-u", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let pidf = root.join("daemon.pid");
        // our own pid: /proc/<pid> exists
        std::fs::write(&pidf, format!("{}", std::process::id())).unwrap();
        let unreadable = |_| -> std::io::Result<Vec<u8>> { Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "EACCES")) };
        let got = alive_impl(&root, true, unreadable).unwrap();
        assert_eq!(got, Some(std::process::id()));
        assert!(pidf.exists(), "inconclusive probe must not remove the pidfile");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Read-succeeded-but-mismatched cmdline is proof of death (pid
    /// recycled): the pidfile is stale and gets removed.
    #[test]
    fn mismatched_cmdline_removes_pidfile() {
        let root = std::env::temp_dir().join(format!("shunt-alive-test-{}-m", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let pidf = root.join("daemon.pid");
        std::fs::write(&pidf, format!("{}", std::process::id())).unwrap();
        let foreign = |_| -> std::io::Result<Vec<u8>> { Ok(argv(&["sleep", "30"])) };
        let got = alive_impl(&root, true, foreign).unwrap();
        assert_eq!(got, None);
        assert!(!pidf.exists(), "proven-dead pidfile must be removed");
        // read-only variant (check_alive path) reports dead but keeps the file
        std::fs::write(&pidf, format!("{}", std::process::id())).unwrap();
        let got = alive_impl(&root, false, foreign).unwrap();
        assert_eq!(got, None);
        assert!(pidf.exists(), "check_alive must never remove a peer's pidfile");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// TOCTOU on the stale-eviction path (M5): a new daemon can write
    /// its live pidfile between our read and our remove — alive()
    /// callers hold no lock. The dead-probe re-read must see the
    /// changed content and leave the file; removing it would strand a
    /// healthy daemon pidfile-less.
    #[test]
    fn dead_probe_keeps_rewritten_pidfile() {
        let root = std::env::temp_dir().join(format!("shunt-alive-test-{}-r", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let pidf = root.join("daemon.pid");
        std::fs::write(&pidf, format!("{}", std::process::id())).unwrap();
        // closure stands in for the race: new daemon rewrites the
        // pidfile after our read, then our probe proves the OLD pid dead
        let pidf2 = pidf.clone();
        let race = move |_| -> std::io::Result<Vec<u8>> {
            std::fs::write(&pidf2, format!("{}", std::process::id() + 1)).unwrap();
            Ok(argv(&["sleep", "30"]))
        };
        let got = alive_impl(&root, true, race).unwrap();
        assert_eq!(got, None);
        assert!(pidf.exists(), "pidfile rewritten by a live daemon must not be removed");
        assert_eq!(std::fs::read_to_string(&pidf).unwrap().trim().parse::<u32>().unwrap(), std::process::id() + 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn probe_dead_when_proc_absent() {
        // pid 2^22-ish: pick a pid guaranteed absent by scanning
        let mut pid = 4_000_000u32;
        while std::path::Path::new(&format!("/proc/{pid}")).exists() {
            pid += 1;
        }
        let probe = super::probe_pid(pid, "shunt", |_| Ok(vec![]));
        assert_eq!(probe, Probe::Dead);
    }
}
