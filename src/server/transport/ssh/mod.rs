//! Real SSH transport: `ssh -p PORT -o BatchMode=yes
//! -o ConnectTimeout=15 -o ServerAliveInterval=…
//! -o ServerAliveCountMax=… -i KEY USER@HOST '<quoted argv>'`,
//! plus `scp` for `copy_to`. ControlMaster mux per client under
//! `$SHUNT_HOME/cm/` (design.md §4.1).
//!
//! - `exec` — remote argv: rc-255 retry, wire-error surfacing
//! - `scp`  — `copy_to` over legacy `scp -O`, same flap discipline

mod exec;
mod scp;
#[cfg(test)]
mod tests;

use super::{cm_dir, run_pre_connect};
use crate::server::registry::ClientEntry;
use anyhow::Result;
use std::path::Path;

/// Real transport: `ssh -p PORT -o BatchMode=yes -o ConnectTimeout=15
/// -o ServerAliveInterval=… -o ServerAliveCountMax=…
/// -i KEY USER@HOST '<quoted argv>'`.
pub struct SshTransport;

/// rc-255 retries per call so one flap never surfaces; an exhausted
/// streak feeds the endpoint-drift counter (design.md §4.1).
const RETRY: u32 = 3;

/// Build the final error for an ssh-level failure (rc 255): bumps
/// the conn_fail streak and appends the endpoint-drift hint at
/// `CONN_FAIL_ESCALATE`. Shared by exec and copy_to so a flap over
/// either channel counts the same way.
fn unreachable_err(client: &ClientEntry, stderr: &str, pre_connect: Option<&str>) -> anyhow::Error {
    let n = crate::server::registry::conn_fail_bump(&client.name);
    // The remediation attempt is reported before the drift hint —
    // the reader should see hook ran, then the streak story
    // (design.md §4.1).
    let hook = pre_connect.map(|note| format!(" [pre_connect {note}]")).unwrap_or_default();
    let hint = if n >= crate::server::registry::CONN_FAIL_ESCALATE {
        if client.endpoint_rotates {
            format!(" [{n} consecutive connect failures — vendor may have reassigned IP/port on restart; check the console, then `shunt client update {} --host … --port …`]", client.name)
        } else {
            format!(" [{n} consecutive connect failures — vendor keeps stable endpoints; box is likely off, asleep, or its sshd is down]")
        }
    } else {
        String::new()
    };
    anyhow::anyhow!("ssh {}@{}:{} unreachable rc=255: {}{}{}", client.ssh.user, client.ssh.host, client.ssh.port, stderr, hook, hint)
}

fn cm_sock(client: &ClientEntry) -> Result<String> {
    Ok(cm_dir()?.join(format!("{}.sock", client.name)).display().to_string())
}

/// One-call `pre_connect` gate (design.md §4.1): runs the hook at
/// most once per call — eagerly at construction when the conn-fail
/// streak is already nonzero (the endpoint is known down, so it
/// fires BEFORE another wasted ConnectTimeout), else on the first
/// rc-255. The outcome never gates retries; it rides the final
/// unreachable error as `note`.
struct PreConnect {
    fired: bool,
    note: Option<String>,
}

impl PreConnect {
    fn new(client: &ClientEntry) -> Self {
        let mut h = Self { fired: client.pre_connect.is_empty(), note: None };
        if !h.fired && crate::server::registry::conn_fail_read(&client.name) > 0 {
            h.on_255(client);
        }
        h
    }

    /// Fire the hook after a reachability failure (no-op once fired).
    fn on_255(&mut self, client: &ClientEntry) {
        if !self.fired {
            self.fired = true;
            self.note = run_pre_connect(client);
        }
    }

    fn note(&self) -> Option<&str> {
        self.note.as_deref()
    }
}

impl SshTransport {
    /// ssh connection flags (-p, BatchMode, timeouts, TOFU known_hosts,
    /// key, user@host, mux). Shared by exec, scp, and the tunnel
    /// manager.
    /// There is no safe fallback for a failed `shunt_home()` (HOME
    /// and SHUNT_HOME both unset — cron/systemd/forced-command
    /// contexts): the old code degraded known_hosts and ControlPath
    /// to /dev/null and chmodded a /tmp fallback, stripping /tmp's
    /// sticky bit. Errors now propagate to the caller instead.
    pub(crate) fn base_args(client: &ClientEntry) -> Result<Vec<String>> {
        // Per-client TOFU store: two rows can legitimately share one
        // host:port behind a NAT gateway while the boxes carry
        // different host keys — a shared file would deadlock the
        // second client on REMOTE HOST IDENTIFICATION HAS CHANGED.
        let kh = crate::server::registry::known_hosts_path(&client.name)?.display().to_string();
        // ControlMaster mux: every exec/scp rides one TCP session per
        // client. Boxes behind startup-limited proxies (`Exceeded
        // MaxStartups`) reject back-to-back handshakes — muxing is the
        // fix, not retries. The socket lives under $SHUNT_HOME/cm/ so
        // nothing touches the user's own ssh config.
        crate::server::fsutil::private_dir(&cm_dir()?)?;
        let mut v = vec!["-p".into(), client.ssh.port.to_string(), "-o".into(), "BatchMode=yes".into(), "-o".into(), format!("ConnectTimeout={}", client.ssh_timeout_s.unwrap_or(15)), "-o".into(), "StrictHostKeyChecking=accept-new".into(), "-o".into(), format!("UserKnownHostsFile={kh}"), "-o".into(), "ControlMaster=auto".into(), "-o".into(), format!("ControlPath={}", cm_sock(client)?), "-o".into(), "ControlPersist=60".into()];
        if !client.ssh.key.is_empty() {
            v.push("-i".into());
            v.push(client.ssh.key.clone());
        }
        Ok(v)
    }

    /// `user@host` — appended by each caller that needs a target
    /// (exec, tunnel). scp deliberately does not get it here: its
    /// sources are paths, and a bare `user@host` parses as a local
    /// file (field-reported bug).
    pub(crate) fn target(client: &ClientEntry) -> String {
        format!("{}@{}", client.ssh.user, client.ssh.host)
    }
}

impl super::Transport for SshTransport {
    fn exec(&self, client: &ClientEntry, argv: &[String], stdin: Option<&str>) -> Result<String> {
        exec::exec(client, argv, stdin)
    }

    fn copy_to(&self, client: &ClientEntry, local: &Path, remote_path: &str) -> Result<()> {
        scp::copy_to(client, local, remote_path)
    }
}
