//! Real SSH transport: `ssh -p PORT -o BatchMode=yes
//! -o ConnectTimeout=15 -o ServerAliveInterval=…
//! -o ServerAliveCountMax=… -i KEY USER@HOST '<quoted argv>'`,
//! plus `scp` for `copy_to`. ControlMaster mux per client under
//! `$SHUNT_HOME/cm/` (design.md §4.1).

use super::{cm_dir, join_argv};
use crate::server::registry::ClientEntry;
use anyhow::{Context, Result};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// Real transport: `ssh -p PORT -o BatchMode=yes -o ConnectTimeout=15
/// -o ServerAliveInterval=… -o ServerAliveCountMax=…
/// -i KEY USER@HOST '<quoted argv>'`.
pub struct SshTransport;

/// Hard deadline for one ssh exec attempt. `ConnectTimeout` only
/// bounds the handshake; a live-but-hung remote command (frozen NFS
/// qroot, remote OOM) would otherwise block the caller forever.
/// ServerAlive catches dead TCP in ~30s; this ceiling catches the
/// rest. Wire ops are short (status/enqueue/push); `logs -f` never
/// crosses ssh. `SHUNT_SSH_EXEC_TIMEOUT_S` overrides (tests).
const EXEC_TIMEOUT_S: u64 = 120;

fn exec_timeout_s() -> u64 {
    std::env::var("SHUNT_SSH_EXEC_TIMEOUT_S").ok().and_then(|s| s.parse().ok()).filter(|&n| n > 0).unwrap_or(EXEC_TIMEOUT_S)
}

/// Poll a spawned child to completion or kill it at the deadline
/// (same pattern as `LocalTransport`'s `timeout_s`).
fn wait_deadline(mut child: std::process::Child, timeout_s: u64, what: &str) -> Result<std::process::Output> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_s);
    loop {
        match child.try_wait()? {
            Some(_) => return child.wait_with_output().with_context(|| format!("wait {what}")),
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait_with_output();
                anyhow::bail!("{what} timed out after {timeout_s}s");
            }
            None => std::thread::sleep(std::time::Duration::from_millis(50)),
        }
    }
}

fn cm_sock(client: &ClientEntry) -> String {
    cm_dir().map(|d| d.join(format!("{}.sock", client.name)).display().to_string()).unwrap_or_else(|_| "/dev/null".into())
}

impl SshTransport {
    /// ssh connection flags (-p, BatchMode, timeouts, TOFU known_hosts,
    /// key, user@host, mux). Shared by exec, scp, and the tunnel
    /// manager.
    pub(crate) fn base_args(client: &ClientEntry) -> Vec<String> {
        // Per-client TOFU store: two rows can legitimately share one
        // host:port behind a NAT gateway while the boxes carry
        // different host keys — a shared file would deadlock the
        // second client on REMOTE HOST IDENTIFICATION HAS CHANGED.
        let kh = crate::server::registry::known_hosts_path(&client.name).map(|p| p.display().to_string()).unwrap_or_else(|_| "/dev/null".into());
        // ControlMaster mux: every exec/scp rides one TCP session per
        // client. Boxes behind startup-limited proxies (`Exceeded
        // MaxStartups`) reject back-to-back handshakes — muxing is the
        // fix, not retries. The socket lives under $SHUNT_HOME/cm/ so
        // nothing touches the user's own ssh config.
        let _ = crate::server::registry::private_dir(&cm_dir().unwrap_or_else(|_| std::path::PathBuf::from("/tmp")));
        let mut v = vec!["-p".into(), client.ssh.port.to_string(), "-o".into(), "BatchMode=yes".into(), "-o".into(), format!("ConnectTimeout={}", client.ssh_timeout_s.unwrap_or(15)), "-o".into(), "StrictHostKeyChecking=accept-new".into(), "-o".into(), format!("UserKnownHostsFile={kh}"), "-o".into(), "ControlMaster=auto".into(), "-o".into(), format!("ControlPath={}", cm_sock(client)), "-o".into(), "ControlPersist=60".into()];
        if !client.ssh.key.is_empty() {
            v.push("-i".into());
            v.push(client.ssh.key.clone());
        }
        v
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
        let mut cmd = Command::new("ssh");
        cmd.args(Self::base_args(client))
            // Half-open TCP detection for exec only — not in base_args:
            // the tunnel sets its own ServerAlive policy and scp
            // (bootstrap) has no liveness requirement.
            .args(["-o", "ServerAliveInterval=15", "-o", "ServerAliveCountMax=2"])
            .arg(Self::target(client))
            .arg(join_argv(argv));
        if stdin.is_some() {
            cmd.stdin(Stdio::piped());
        } else {
            cmd.stdin(Stdio::null());
        }
        // rc 255 = ssh-level failure (connect/refused/`Exceeded
        // MaxStartups` flap). Retry a few times inside the call so a
        // single flap never bubbles up; only an exhausted streak
        // bumps the endpoint-drift counter (design.md §4.1).
        const RETRY: u32 = 3;
        for attempt in 1..=RETRY {
            let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().context("spawn ssh")?;
            if let Some(body) = stdin {
                child.stdin.as_mut().expect("stdin piped").write_all(body.as_bytes())?;
            }
            drop(child.stdin.take());
            // A hard deadline that fires means the remote command hung
            // while TCP stayed live — not a flap, so bail without
            // retry and without bumping the conn_fail streak.
            let out = wait_deadline(child, exec_timeout_s(), "ssh exec")?;
            if out.status.success() {
                crate::server::registry::conn_fail_reset(&client.name);
                return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
            }
            let rc = out.status.code().unwrap_or(-1);
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            if rc != 255 || attempt == RETRY {
                if rc == 255 {
                    let n = crate::server::registry::conn_fail_bump(&client.name);
                    let hint = if n >= crate::server::registry::CONN_FAIL_ESCALATE {
                        if client.endpoint_rotates {
                            format!(" [{n} consecutive connect failures — vendor may have reassigned IP/port on restart; check the console, then `shunt client update {} --host … --port …`]", client.name)
                        } else {
                            format!(" [{n} consecutive connect failures — vendor keeps stable endpoints; box is likely off, asleep, or its sshd is down]")
                        }
                    } else {
                        String::new()
                    };
                    anyhow::bail!("ssh {}@{}:{} unreachable rc={}: {}{}", client.ssh.user, client.ssh.host, client.ssh.port, rc, stderr, hint);
                }
                anyhow::bail!("ssh {}@{}:{} rc={}: {}", client.ssh.user, client.ssh.host, client.ssh.port, rc, stderr);
            }
            std::thread::sleep(std::time::Duration::from_millis(1500));
        }
        unreachable!("retry loop always returns");
    }

    fn copy_to(&self, client: &ClientEntry, local: &Path, remote_path: &str) -> Result<()> {
        // scp shares the ssh flags — including the ControlMaster mux,
        // so bulk copies ride the same single connection (design.md
        // §4.1). -P for port (scp) vs -p (ssh): reuse base_args but
        // drop its leading `-p PORT` pair and trailing `user@host`
        // (scp sources are paths; a bare user@host reads as a local file).
        // base_args is flags-only now: drop its leading `-p PORT`
        // (scp spells it -P), then local source + user@host:path.
        let mut args = vec!["-P".into(), client.ssh.port.to_string()];
        args.extend(Self::base_args(client).into_iter().skip(2));
        args.push(local.display().to_string());
        args.push(format!("{}@{}:{}", client.ssh.user, client.ssh.host, remote_path));
        let out = Command::new("scp").args(&args).stdin(Stdio::null()).output().context("spawn scp")?;
        if !out.status.success() {
            anyhow::bail!("scp to {} failed: {}", client.name, String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::{local_client, Transport};
    use super::SshTransport;
    use crate::server::registry::ClientEntry;

    /// Env mutation (PATH, timeout override) is process-global —
    /// serialize the fake-ssh tests.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Install a fake `ssh` ahead on PATH; returns the tmp dir.
    /// `script` is the whole shell body; argv reaches it as $@.
    fn fake_ssh(script: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("shunt-fakessh-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ssh");
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let old = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{old}", dir.display()));
        dir
    }

    fn restore_path() {
        if let Ok(p) = std::env::var("PATH") {
            // Drop the faked leading entry.
            std::env::set_var("PATH", p.split_once(':').map(|(_, rest)| rest).unwrap_or(&p));
        }
    }

    fn ssh_client() -> ClientEntry {
        let mut c = local_client();
        c.local = false;
        c.ssh.host = "127.0.0.1".into();
        c.ssh.user = "u".into();
        c
    }

    #[test]
    fn ssh_exec_passes_serveralive() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("shunt-fakessh-out-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let argfile = dir.join("argv");
        std::env::set_var("FAKE_ARGV", &argfile);
        fake_ssh("printf '%s\\n' \"$@\" > \"$FAKE_ARGV\"; exit 0");
        let out = SshTransport.exec(&ssh_client(), &["status".into(), "--json".into()], None).unwrap();
        assert_eq!(out, "");
        let argv = std::fs::read_to_string(&argfile).unwrap();
        assert!(argv.contains("ServerAliveInterval=15"), "{argv}");
        assert!(argv.contains("ServerAliveCountMax=2"), "{argv}");
        std::env::remove_var("FAKE_ARGV");
        restore_path();
    }

    #[test]
    fn ssh_exec_deadline_kills_hung_remote() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("SHUNT_SSH_EXEC_TIMEOUT_S", "1");
        fake_ssh("sleep 30");
        let err = SshTransport.exec(&ssh_client(), &["status".into(), "--json".into()], None).unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
        std::env::remove_var("SHUNT_SSH_EXEC_TIMEOUT_S");
        restore_path();
    }
}
