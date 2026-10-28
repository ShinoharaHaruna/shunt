//! Real SSH transport: managed key auth or password-only auth via a
//! generated `SSH_ASKPASS` script (`sshpass -e` only on pre-8.4 ssh),
//! plus `scp` for `copy_to`. ControlMaster mux per client under
//! `$SHUNT_HOME/cm/` (design.md §4.1).
//!
//! - `exec` — remote argv: rc-255 retry, wire-error surfacing
//! - `scp`  — `copy_to` over legacy `scp -O`, same flap discipline

mod exec;
mod scp;
#[cfg(test)]
mod tests;

use super::{cm_dir, run_pre_connect, shell_quote};
use crate::server::registry::ClientEntry;
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Real transport: key-auth rows use `BatchMode=yes` and may add `-i KEY`;
/// password-only rows use sshpass and password-auth flags.
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
        if !h.fired && crate::server::registry::conn_fail_streak(client) > 0 {
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
    /// ssh connection flags (-p, auth mode, timeouts, TOFU known_hosts,
    /// key, mux) followed by per-client options. Shared by exec, scp,
    /// and ad-hoc SSH uses.
    pub(crate) fn base_args(client: &ClientEntry) -> Result<Vec<String>> {
        let mut args = Self::managed_args(client)?;
        client.ssh.append_options(&mut args);
        Ok(args)
    }

    /// Build only shunt-managed flags. Callers that add managed options
    /// of their own must append `ssh.options` after those too.
    /// There is no safe fallback for a failed `shunt_home()` (HOME
    /// and SHUNT_HOME both unset — cron/systemd/forced-command
    /// contexts): the old code degraded known_hosts and ControlPath
    /// to /dev/null and chmodded a /tmp fallback, stripping /tmp's
    /// sticky bit. Errors now propagate to the caller instead.
    pub(crate) fn managed_args(client: &ClientEntry) -> Result<Vec<String>> {
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
        let mut v = vec!["-p".into(), client.ssh.port.to_string()];
        if client.ssh.password_only() {
            v.extend(["-o".into(), "PreferredAuthentications=password".into(), "-o".into(), "PubkeyAuthentication=no".into(), "-o".into(), "NumberOfPasswordPrompts=1".into()]);
        } else {
            v.extend(["-o".into(), "BatchMode=yes".into()]);
        }
        v.extend(["-o".into(), format!("ConnectTimeout={}", client.ssh_timeout_s.unwrap_or(15)), "-o".into(), "StrictHostKeyChecking=accept-new".into(), "-o".into(), format!("UserKnownHostsFile={kh}"), "-o".into(), "ControlMaster=auto".into(), "-o".into(), format!("ControlPath={}", cm_sock(client)?), "-o".into(), "ControlPersist=60".into()]);
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

/// Wrap password-only SSH tools without exposing the secret in argv.
/// OpenSSH ≥ 8.4 answers the password prompt through a generated
/// `cm/<name>.askpass` script (`SSH_ASKPASS_REQUIRE=force`); only
/// older ssh keeps the `sshpass -e` wrapper — correct, but its pty
/// teardown SIGHUPs the ssh ProxyCommand child, killing the mux
/// master's transport so ControlPersist never persists and every op
/// pays a fresh handshake (design.md §4.1). scp and the tunnel ride
/// this too: scp's internal ssh child inherits the env. The
/// prompt-capable transport is explicit; ordinary key/agent auth
/// keeps its existing direct spawn path (§4.1).
pub(crate) fn ssh_tool_cmd(client: &ClientEntry, tool: &str) -> Result<Command> {
    if !client.ssh.password_only() {
        return Ok(Command::new(tool));
    }
    if askpass_require_supported() {
        // DISPLAY only needs to be SET (REQUIRE=force never opens an
        // X connection). BatchMode must stay off password rows — it
        // suppresses SSH_ASKPASS entirely; managed_args already
        // confines it to the key branch.
        let mut cmd = Command::new(tool);
        cmd.env("SSH_ASKPASS", askpass_script(client)?).env("SSH_ASKPASS_REQUIRE", "force").env("DISPLAY", ":0");
        return Ok(cmd);
    }
    let have_sshpass = Command::new("sh").args(["-c", "command -v sshpass >/dev/null 2>&1"]).status().map(|s| s.success()).unwrap_or(false);
    anyhow::ensure!(have_sshpass, "ssh.password is set with empty ssh.key and ssh is older than OpenSSH 8.4 — install `sshpass` or set ssh.key");
    let mut cmd = Command::new("sshpass");
    cmd.args(["-e", tool]).env("SSHPASS", &client.ssh.password);
    Ok(cmd)
}

/// `SSH_ASKPASS_REQUIRE` exists in OpenSSH ≥ 8.4 (Sep 2020); below it
/// the askpass env is ignored without a tty. Probed once per process
/// via `ssh -V` (prints on stderr); unparseable output takes the
/// sshpass fallback rather than a guessed version. The
/// `SHUNT_SSH_VERSION` override is read per call — the OnceLock
/// caches only the real binary's answer, so tests pin the branch
/// deterministically regardless of what an earlier call already
/// cached in this process (same seam pattern as
/// `SHUNT_SSH_EXEC_TIMEOUT_S`).
fn askpass_require_supported() -> bool {
    if let Ok(v) = std::env::var("SHUNT_SSH_VERSION") {
        return parse_openssh_version(&v).is_some_and(|v| v >= (8, 4));
    }
    static SUPPORTS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SUPPORTS.get_or_init(|| Command::new("ssh").arg("-V").stdin(std::process::Stdio::null()).output().ok().and_then(|o| parse_openssh_version(&String::from_utf8_lossy(&o.stderr))).is_some_and(|v| v >= (8, 4)))
}

/// `OpenSSH_X.Y[pN][ …]` → (X, Y): the version is the first
/// whitespace token of the `ssh -V` stderr line, and the minor may
/// carry a `pN` patch suffix.
fn parse_openssh_version(line: &str) -> Option<(u32, u32)> {
    let v = line.split_whitespace().find_map(|t| t.strip_prefix("OpenSSH_"))?;
    let mut parts = v.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.chars().take_while(char::is_ascii_digit).collect::<String>().parse().ok()?;
    Some((major, minor))
}

/// `cm/<name>.askpass`: ssh execs this to answer the password
/// prompt — a generated script keeps the plaintext out of argv and
/// out of the user's own askpass setup (it already sits plaintext in
/// the registry row, so exposure is unchanged). Rewritten only when
/// the secret changes: the rc-255 retry loop walks this code every
/// attempt. 0700 like the cm dir itself.
fn askpass_script(client: &ClientEntry) -> Result<PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let dir = cm_dir()?;
    crate::server::fsutil::private_dir(&dir)?;
    let path = dir.join(format!("{}.askpass", client.name));
    let body = format!("#!/bin/sh\nprintf '%s\\n' {}\n", shell_quote(&client.ssh.password));
    if std::fs::read_to_string(&path).ok().as_deref() == Some(body.as_str()) {
        return Ok(path);
    }
    // Install via the pid-tagged sibling: a read mid-rewrite would
    // hand ssh a torn askpass script and fail auth.
    let tmp = crate::server::fsutil::tmp_sibling(&path);
    {
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o700).open(&tmp)?;
        f.write_all(body.as_bytes())?;
    }
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o700))?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

impl super::Transport for SshTransport {
    fn exec(&self, client: &ClientEntry, argv: &[String], stdin: Option<&str>) -> Result<String> {
        exec::exec(client, argv, stdin)
    }

    fn copy_to(&self, client: &ClientEntry, local: &Path, remote_path: &str) -> Result<()> {
        scp::copy_to(client, local, remote_path)
    }
}
