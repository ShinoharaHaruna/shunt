//! Liveness by cmdline identity: a recycled pid must never pass
//! for a live tunnel.

use crate::proto::proc::{self, ProcProbe};
use crate::server::registry::ClientEntry;

/// `user@host` needle for cmdline identity — only when the row
/// carries a real endpoint (local rows have neither field).
pub(super) fn client_target(client: &ClientEntry) -> Option<String> {
    if client.local || client.ssh.user.is_empty() || client.ssh.host.is_empty() {
        None
    } else {
        Some(format!("{}@{}", client.ssh.user, client.ssh.host))
    }
}

/// The tunnel needle for `proto::proc::probe`: split argv → our
/// tunnel when argv[0]'s basename is `ssh` AND the recorded needles
/// are present: `-R` followed by the recorded port (when a port file
/// exists) and the `user@host` target (when the caller knows the
/// client row). A bare `ssh -R` match accepted ANY reverse forward —
/// a stranger's tunnel recycling our recorded pid would then pass
/// liveness and `down` would TERM it. Whole-element matching, same
/// discipline as the daemon's `is_daemon_cmdline` (design.md §3.4).
pub(super) fn is_tunnel_cmdline(argv: &[&[u8]], port: Option<u16>, target: Option<&str>) -> bool {
    let Some(argv0) = argv.first() else { return false };
    if proc::argv0_base(argv0) != b"ssh" {
        return false;
    }
    let r_ok = match port {
        // `-R <port>` as an adjacent pair — the exact form `up` spawns.
        Some(p) => {
            let ps = p.to_string();
            argv.windows(2).any(|w| w[0] == b"-R" && w[1] == ps.as_bytes())
        }
        None => argv.iter().any(|a| *a == b"-R"),
    };
    if !r_ok {
        return false;
    }
    match target {
        Some(t) => argv.contains(&t.as_bytes()),
        None => true,
    }
}

pub(super) fn pid_alive(pid: u32, port: Option<u16>, target: Option<&str>) -> bool {
    // Cmdline identity, not bare /proc existence: a recycled pid must
    // not pass for a live tunnel (status lies, `up` early-bails, and
    // `down` would signal a foreign process). Tunnel polarity maps
    // `Unknown` to dead — opposite of the daemon's fail-safe: a tunnel
    // that cannot be verified is restarted/healed, never trusted.
    matches!(proc::probe(pid, proc::read_cmdline, |argv| is_tunnel_cmdline(argv, port, target)), ProcProbe::Live)
}
