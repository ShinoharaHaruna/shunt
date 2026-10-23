//! `/proc` liveness middleware (design.md §3.4): recorded pid →
//! read `/proc/<pid>/cmdline` → NUL-split argv → whole-element
//! identity needle → `ProcProbe`. Both probe consumers — the client
//! daemon's pidfile discipline and the server tunnel's ssh pid —
//! share this ladder; they differ only in the needle (what counts
//! as "ours") and in the polarity each maps `Unknown` to:
//!
//!   daemon — `Unknown` reports ALIVE: deleting a live pidfile or
//!            double-spawning a daemon is worse than a false live.
//!   tunnel — `Unknown` reports dead:  a tunnel that cannot be
//!            verified is restarted/healed, never trusted.
//!
//! What stays domain-local by design: the pidfile itself (daemon's
//! `daemon.pid` + TOCTOU re-read before eviction, tunnel's
//! `tunnels/<name>.pid` + lock ordering) — each side owns its own
//! file lifecycle. Process-group liveness (`client::exec::pgid_alive`)
//! is a different discipline entirely — kill(2) probe on a pgid, no
//! cmdline — and does not come through here.

use std::io;
use std::path::Path;

/// The liveness ladder's verdict for one pid. `Dead` is PROOF of
/// death only: `/proc/<pid>` is absent, or its cmdline was read and
/// does not match the needle (pid recycled — the recorded process is
/// provably gone). A cmdline READ FAILURE (EACCES on a different-uid
/// process, transient proc pressure) is `Unknown` — inconclusive;
/// the caller's domain picks the safe polarity.
#[derive(Debug, PartialEq, Eq)]
pub enum ProcProbe {
    Dead,
    Live,
    Unknown,
}

/// Read `/proc/<pid>/cmdline` — the real reader production call
/// sites pass to `probe`; tests inject their own.
pub fn read_cmdline(pid: u32) -> io::Result<Vec<u8>> {
    std::fs::read(format!("/proc/{pid}/cmdline"))
}

/// NUL-separated `/proc/<pid>/cmdline` → argv elements. Empty
/// elements (a trailing NUL is normal) are dropped, so `argv[1]` is
/// the second real element — the subcommand slot.
pub fn split_argv(cmdline: &[u8]) -> Vec<&[u8]> {
    cmdline.split(|&b| b == 0).filter(|a| !a.is_empty()).collect()
}

/// argv[0] → its basename (`/usr/bin/ssh` → `ssh`). Identity checks
/// match the basename, never the full path — install prefixes differ.
pub fn argv0_base(argv0: &[u8]) -> &[u8] {
    argv0.rsplit(|&b| b == b'/').next().unwrap_or(argv0)
}

/// Probe one recorded pid: `/proc/<pid>` absent → `Dead`; cmdline
/// read + needle match → `Live`; read + mismatch → `Dead` (recycled
/// pid); read error → `Unknown`. `read_cmdline` is injected so tests
/// can reach EACCES/race paths a real `/proc` cannot produce on
/// demand; `needle` receives the split argv and decides identity.
pub fn probe(pid: u32, read_cmdline: impl Fn(u32) -> io::Result<Vec<u8>>, needle: impl Fn(&[&[u8]]) -> bool) -> ProcProbe {
    if !Path::new(&format!("/proc/{pid}")).exists() {
        return ProcProbe::Dead;
    }
    match read_cmdline(pid) {
        Ok(c) if needle(&split_argv(&c)) => ProcProbe::Live,
        Ok(_) => ProcProbe::Dead,
        Err(_) => ProcProbe::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pid whose `/proc` entry is absent is provably dead — the
    /// needle is never even consulted.
    #[test]
    fn absent_proc_is_dead() {
        let mut pid = 4_000_000u32;
        while Path::new(&format!("/proc/{pid}")).exists() {
            pid += 1;
        }
        assert_eq!(probe(pid, |_| Ok(vec![]), |_| true), ProcProbe::Dead);
    }

    /// On a live pid (our own), the verdict follows the needle:
    /// match → Live, mismatch → Dead (a readable foreign cmdline is
    /// what a recycled pid looks like).
    #[test]
    fn live_pid_maps_needle() {
        let pid = std::process::id();
        assert_eq!(probe(pid, read_cmdline, |_| true), ProcProbe::Live);
        assert_eq!(probe(pid, read_cmdline, |_| false), ProcProbe::Dead);
    }

    /// A live pid whose cmdline cannot be read (EACCES, transient
    /// proc pressure) is inconclusive — never Dead, so neither
    /// polarity can mistake it for proof.
    #[test]
    fn read_failure_is_unknown() {
        let pid = std::process::id();
        let eacces = |_| -> io::Result<Vec<u8>> { Err(io::Error::new(io::ErrorKind::PermissionDenied, "EACCES")) };
        assert_eq!(probe(pid, eacces, |_| true), ProcProbe::Unknown);
    }

    /// NUL-split drops empty elements, so the subcommand slot is
    /// argv[1] whether or not the cmdline ends with a trailing NUL.
    #[test]
    fn argv_split_skips_empty_elements() {
        assert_eq!(split_argv(b"shunt\0daemon\0"), vec![b"shunt".as_slice(), b"daemon".as_slice()]);
        assert_eq!(split_argv(b"shunt\0daemon"), vec![b"shunt".as_slice(), b"daemon".as_slice()]);
        assert_eq!(split_argv(b""), Vec::<&[u8]>::new());
        assert_eq!(split_argv(b"\0\0"), Vec::<&[u8]>::new());
    }

    #[test]
    fn basename_strips_directories() {
        assert_eq!(argv0_base(b"/usr/bin/ssh"), b"ssh");
        assert_eq!(argv0_base(b"ssh"), b"ssh");
        assert_eq!(argv0_base(b"/opt/shunt/bin/shunt-2.0-rc1"), b"shunt-2.0-rc1");
    }
}
