//! Daemon liveness and self-daemonization: pidfile + `/proc/<pid>`
//! discipline (design.md §3.4).

use anyhow::{Context, Result};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::client::queue;
use crate::proto::proc::{self, ProcProbe};

/// True when argv[0]'s basename is a shunt-family binary name:
/// `shunt`, `shuntd`, or `shunt-<anything>` (versioned installs like
/// `shunt-1.2`). Sibling daemons on one box (design.md §3.11 qroot
/// registry) may run under any of these — a `shuntd` daemon probed by
/// `shunt` must read Live, not Dead, or `busy_peers` sweeps its
/// registry entry and idle-shutdown fires under live work. A
/// `-shunt` SUFFIX (`foo-shunt`) stays rejected: the name must start
/// with `shunt` to count as family.
fn is_shunt_family(base: &[u8]) -> bool {
    base == b"shunt" || base == b"shuntd" || base.starts_with(b"shunt-")
}

/// The daemon needle for `proto::proc::probe`: split argv → this is
/// a shunt daemon when argv[0]'s basename is shunt-family
/// (`is_shunt_family`) AND argv[1] — the subcommand slot; this CLI has
/// no pre-subcommand global flags — is exactly `queue::DAEMON_IDENTITY`.
/// Anchoring at argv[1] keeps a recycled pid running `shunt logs
/// daemon`/`kill daemon` (pattern positional) from false-identifying
/// as the daemon. Basename-family + whole-element beats both a
/// substring test and an exact-name match: an unrelated `foo-shunt
/// daemon` cannot false-match, while a differently-named sibling
/// daemon still does (design.md §3.4, §3.11).
fn is_daemon_cmdline(argv: &[&[u8]]) -> bool {
    let Some(argv0) = argv.first() else {
        return false;
    };
    if !is_shunt_family(proc::argv0_base(argv0)) {
        return false;
    }
    argv.get(1).is_some_and(|a| *a == queue::DAEMON_IDENTITY.as_bytes())
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
    match proc::probe(pid, read_cmdline, is_daemon_cmdline) {
        ProcProbe::Live | ProcProbe::Unknown => Ok(Some(pid)),
        ProcProbe::Dead => {
            if evict_stale {
                // TOCTOU: `run()` writes its pidfile after taking
                // daemon.lock while alive() callers hold no lock, so a
                // new daemon's live pidfile can land between our read
                // and this remove. Re-read and remove only when the
                // content is still the pid we proved dead; a changed
                // pidfile is owned by a live daemon (design.md §3.4).
                let still = std::fs::read_to_string(&pidf).ok().and_then(|s| s.trim().parse::<u32>().ok()) == Some(pid);
                if still {
                    // The §3.5a capability files die with the pid
                    // they describe; leave them and a later `restart`
                    // would gate on a dead daemon's version.
                    super::drop_daemon_files(root);
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
    alive_impl(root, true, proc::read_cmdline)
}

/// Read-only liveness for sibling qroots (`busy_peers`): identical
/// probe, but never removes `daemon.pid` — the peer daemon owns it.
/// Only a genuinely missing pidfile proves absence; unreadable or malformed
/// peer state must not be silently swept by the box-level shutdown gate.
pub fn check_alive(root: &Path) -> Result<Option<u32>> {
    let pidfile = root.join("daemon.pid");
    let content = match std::fs::read_to_string(&pidfile) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("cannot read peer pidfile {}", pidfile.display())),
    };
    let pid = content.trim().parse::<u32>().with_context(|| format!("invalid peer pidfile {}", pidfile.display()))?;
    anyhow::ensure!(pid > 0, "invalid peer pidfile {}", pidfile.display());
    match proc::probe(pid, proc::read_cmdline, is_daemon_cmdline) {
        ProcProbe::Live | ProcProbe::Unknown => Ok(Some(pid)),
        ProcProbe::Dead => Ok(None),
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

/// Outcome of `restart`. `HandedOff` sent SIGUSR2 to a live,
/// handoff-capable daemon; `Started` means no live daemon was found
/// and a fresh one was spawned (already the new binary).
pub enum Restart {
    HandedOff { pid: u32, from_ver: String },
    Started { pid: u32 },
}

/// Handoff-restart the daemon (§3.5a): a live daemon gets SIGUSR2 —
/// it re-execs its own image in place, adopting in-flight tasks via
/// the §3.6 ppid gate, so the queue is never unmanaged and no task
/// is drained or killed. `daemon.ver` is the capability gate: an
/// image that predates the handler would be KILLED by a bare USR2
/// (default disposition), so its absence refuses rather than
/// upgrades. A dead daemon falls through to `start`, which launches
/// the new binary anyway.
pub fn restart(root: &Path) -> Result<Restart> {
    let Some(pid) = alive(root)? else {
        return start(root).map(|pid| Restart::Started { pid });
    };
    let ver = std::fs::read_to_string(root.join("daemon.ver")).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let Some(from_ver) = ver else {
        anyhow::bail!("daemon (pid {pid}) predates handoff support — no daemon.ver, and SIGUSR2 would kill it; upgrade the binary with `shunt upgrade` and retry, or stop+start");
    };
    let rc = unsafe { libc::kill(pid as i32, libc::SIGUSR2) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("cannot SIGUSR2 daemon pid {pid}"));
    }
    Ok(Restart::HandedOff { pid, from_ver })
}

#[cfg(test)]
mod tests {
    use super::{alive_impl, is_daemon_cmdline};

    fn argv<'a>(parts: &[&'a str]) -> Vec<&'a [u8]> {
        parts.iter().map(|p| p.as_bytes()).collect()
    }

    /// A NUL-separated cmdline as `/proc` serves it — for readers
    /// injected into `alive_impl`.
    fn cmdline(parts: &[&str]) -> Vec<u8> {
        let mut v = Vec::new();
        for p in parts {
            v.extend_from_slice(p.as_bytes());
            v.push(0);
        }
        v
    }

    /// Identity = shunt-family argv[0] basename + argv[1] ==
    /// `daemon` (design.md §3.4): renamed binaries and cross-named
    /// siblings all match, unrelated `foo-shunt daemon` does not, and
    /// `daemon` at any other argv position (pattern positional of
    /// `logs`/`kill`/`cancel`) must not match.
    #[test]
    fn cmdline_identity_basename_plus_element() {
        assert!(is_daemon_cmdline(&argv(&["/usr/bin/shunt", "daemon", "--root", "/q"])));
        assert!(is_daemon_cmdline(&argv(&["shunt", "daemon", "--root", "/q"])));
        // sibling daemon under another shunt-family name (M6): a
        // `shuntd`/`shunt-1.2` daemon probed by `shunt` must read Live
        assert!(is_daemon_cmdline(&argv(&["shuntd", "daemon", "--root", "/q"])));
        assert!(is_daemon_cmdline(&argv(&["shunt-1.2", "daemon", "--root", "/q"])));
        assert!(is_daemon_cmdline(&argv(&["/opt/shunt/bin/shunt-2.0-rc1", "daemon"])));
        // basename mismatch: foreign binary must not match
        assert!(!is_daemon_cmdline(&argv(&["/usr/bin/foo-shunt", "daemon"])));
        assert!(!is_daemon_cmdline(&argv(&["my-shunt", "daemon"])));
        assert!(!is_daemon_cmdline(&argv(&["shuntfoo", "daemon"])));
        // `daemon` at a later argv position is a pattern, not the subcommand
        assert!(!is_daemon_cmdline(&argv(&["shunt", "logs", "daemon"])));
        assert!(!is_daemon_cmdline(&argv(&["shunt", "kill", "daemon"])));
        // `daemon` only as a substring of an argv element
        assert!(!is_daemon_cmdline(&argv(&["shunt", "daemonesque"])));
        assert!(!is_daemon_cmdline(&argv(&["shunt", "--root", "/q"])));
        assert!(!is_daemon_cmdline(&[]));
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
        let foreign = |_| -> std::io::Result<Vec<u8>> { Ok(cmdline(&["sleep", "30"])) };
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
            Ok(cmdline(&["sleep", "30"]))
        };
        let got = alive_impl(&root, true, race).unwrap();
        assert_eq!(got, None);
        assert!(pidf.exists(), "pidfile rewritten by a live daemon must not be removed");
        assert_eq!(std::fs::read_to_string(&pidf).unwrap().trim().parse::<u32>().unwrap(), std::process::id() + 1);
        let _ = std::fs::remove_dir_all(&root);
    }
}
