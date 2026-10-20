//! Real SSH transport: `ssh -p PORT -o BatchMode=yes
//! -o ConnectTimeout=15 -o ServerAliveInterval=…
//! -o ServerAliveCountMax=… -i KEY USER@HOST '<quoted argv>'`,
//! plus `scp` for `copy_to`. ControlMaster mux per client under
//! `$SHUNT_HOME/cm/` (design.md §4.1).

use super::{cm_dir, exec_timeout_s, join_argv, shell_quote, wait_deadline};
use crate::server::registry::ClientEntry;
use anyhow::{Context, Result};
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

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
fn unreachable_err(client: &ClientEntry, stderr: &str) -> anyhow::Error {
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
    anyhow::anyhow!("ssh {}@{}:{} unreachable rc=255: {}{}", client.ssh.user, client.ssh.host, client.ssh.port, stderr, hint)
}

fn cm_sock(client: &ClientEntry) -> Result<String> {
    Ok(cm_dir()?.join(format!("{}.sock", client.name)).display().to_string())
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
        let mut cmd = Command::new("ssh");
        cmd.args(Self::base_args(client)?)
            // Half-open TCP detection — not in base_args because the
            // tunnel sets its own ServerAlive policy. scp spells the
            // same opts via copy_to below (design.md §5).
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
        for attempt in 1..=RETRY {
            // process_group(0): the child's pgid equals its pid so a
            // wait_deadline timeout can kill the whole group — a bare
            // kill() would orphan grandchildren holding the pipes.
            let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0).spawn().context("spawn ssh")?;
            if let Some(body) = stdin {
                // Write from a thread: a remote that never reads stdin
                // while filling stdout would otherwise deadlock the
                // write — the pipe readers only start in wait_deadline.
                let mut w = child.stdin.take().expect("stdin piped");
                let body = body.to_string();
                std::thread::spawn(move || {
                    let _ = w.write_all(body.as_bytes());
                });
            }
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
            // rc 255 is the ssh-layer signal — it routes to retry /
            // unreachable_err FIRST, regardless of stdout content. A
            // dead or mux-replaying box can leave a stale
            // `{"ok":false}` line on a 255 exit; reading it as a wire
            // error would skip the retry and the conn_fail bump and
            // hide the dead endpoint from the drift watchdog
            // (design.md §4.1).
            if rc == 255 {
                if attempt == RETRY {
                    return Err(unreachable_err(client, &stderr));
                }
                std::thread::sleep(std::time::Duration::from_millis(1500));
                continue;
            }
            // Wire ops report failures as `{"ok":false,"error":"…"}`
            // on stdout with a nonzero exit — surface that message.
            // Raw argv (bootstrap's `sh -c`, probes) prints no wire
            // JSON, so extract fails and we fall through to rc+stderr.
            if let Ok(v) = crate::proto::wire::extract(&String::from_utf8_lossy(&out.stdout)) {
                if v.get("ok").and_then(|o| o.as_bool()) == Some(false) {
                    if let Some(e) = v.get("error").and_then(|e| e.as_str()) {
                        anyhow::bail!("remote exec failed: {e}");
                    }
                }
            }
            anyhow::bail!("ssh {}@{}:{} rc={}: {}", client.ssh.user, client.ssh.host, client.ssh.port, rc, stderr);
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
        args.extend(Self::base_args(client)?.into_iter().skip(2));
        // Same double bound as exec (design.md §5): half-open TCP
        // dies in ~30s instead of riding the full hard deadline.
        args.extend(["-o".into(), "ServerAliveInterval=15".into(), "-o".into(), "ServerAliveCountMax=2".into()]);
        args.push(local.display().to_string());
        // The remote side of scp runs through the remote login
        // shell — the qroot-derived path must be quoted or spaces /
        // `$()` / backticks break (or inject) the remote command.
        args.push(format!("{}@{}:{}", client.ssh.user, client.ssh.host, shell_quote(remote_path)));
        // Same contract as exec: retry rc-255 flaps, bounded by the
        // hard deadline (a hang is not a flap — bail without retry or
        // streak bump), feed the drift counter only on exhaustion.
        // Bootstrap pushes the binary through here, so a single
        // `Exceeded MaxStartups` flap must not fail the copy.
        for attempt in 1..=RETRY {
            let child = Command::new("scp").args(&args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0).spawn().context("spawn scp")?;
            let out = wait_deadline(child, exec_timeout_s(), "scp")?;
            if out.status.success() {
                crate::server::registry::conn_fail_reset(&client.name);
                return Ok(());
            }
            let rc = out.status.code().unwrap_or(-1);
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            if rc != 255 || attempt == RETRY {
                if rc == 255 {
                    return Err(unreachable_err(client, &stderr));
                }
                anyhow::bail!("scp to {} failed rc={}: {}", client.name, rc, stderr);
            }
            std::thread::sleep(std::time::Duration::from_millis(1500));
        }
        unreachable!("retry loop always returns");
    }
}

#[cfg(test)]
mod tests {
    use super::super::{local_client, Transport};
    use super::SshTransport;
    use crate::server::registry::ClientEntry;

    /// Env mutation (PATH, timeout override) is process-global —
    /// serialize the fake-ssh tests. Tests that also touch
    /// SHUNT_HOME additionally hold the registry's HOME_LOCK so
    /// registry-side env users can't observe their value.
    use super::super::ENV_LOCK;

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
    fn ssh_exec_drains_big_stdout() {
        let _g = ENV_LOCK.lock().unwrap();
        // >64 KiB on stdout deadlocks an undrained pipe: the remote
        // blocks on write, never exits, and the deadline would fire
        // on a healthy-but-verbose command (`logs -n 200`).
        fake_ssh("yes | head -c 200000");
        let out = SshTransport.exec(&ssh_client(), &["logs".into(), "-n".into(), "200".into()], None).unwrap();
        assert_eq!(out.len(), 200000);
        restore_path();
    }

    #[test]
    fn ssh_exec_surfaces_remote_wire_error() {
        let _g = ENV_LOCK.lock().unwrap();
        // Wire ops fail as `{"ok":false,"error":"…"}` on stdout + rc 1;
        // the message must reach the caller, not die with the pipe.
        fake_ssh("printf '%s\\n' '{\"protocol\":1,\"ok\":false,\"error\":\"sequence space exhausted\"}'; exit 1");
        let err = SshTransport.exec(&ssh_client(), &["enqueue".into()], None).unwrap_err();
        assert!(err.to_string().contains("sequence space exhausted"), "{err}");
        restore_path();
    }

    /// A stale `{"ok":false}` line on an rc-255 exit must NOT take
    /// the wire-error path: rc 255 is the ssh-layer signal and always
    /// routes to retry/conn_fail (a mux replaying buffered output or
    /// a wedged remote echoing an old reply must not mask a dead box).
    #[test]
    fn ssh_exec_255_with_wire_line_still_retries() {
        let _h = crate::server::registry::tests::HOME_LOCK.lock().unwrap();
        let _g = ENV_LOCK.lock().unwrap();
        let home = std::env::temp_dir().join(format!("shunt-ssh-home255-{}", std::process::id()));
        std::env::set_var("SHUNT_HOME", &home);
        let dir = std::env::temp_dir().join(format!("shunt-fakessh-255-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let count = dir.join("count");
        fake_ssh(&format!("printf '%s\\n' '{{\"protocol\":1,\"ok\":false,\"error\":\"stale\"}}'; n=$(cat {0} 2>/dev/null || echo 0); echo $((n+1)) > {0}; exit 255", count.display()));
        let err = SshTransport.exec(&ssh_client(), &["status".into(), "--json".into()], None).unwrap_err();
        assert!(err.to_string().contains("unreachable rc=255"), "{err}");
        assert_eq!(std::fs::read_to_string(&count).unwrap().trim(), "3", "rc 255 flaps must retry despite the wire line");
        std::env::remove_var("SHUNT_HOME");
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

    /// A failed shunt_home()/cm-dir setup must reach the caller as an
    /// error — not degrade to /dev/null ControlPath + a chmodded /tmp
    /// (the fallback once stripped /tmp's sticky bit).
    #[test]
    fn base_args_errs_when_home_unusable() {
        let _h = crate::server::registry::tests::HOME_LOCK.lock().unwrap();
        std::env::set_var("SHUNT_HOME", "/proc/shunt-cannot-exist");
        assert!(SshTransport::base_args(&ssh_client()).is_err());
        std::env::remove_var("SHUNT_HOME");
    }

    /// Install a fake `scp` ahead on PATH (same pattern as `fake_ssh`).
    fn fake_scp(script: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("shunt-fakescp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("scp");
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

    #[test]
    fn scp_retries_255_flaps_then_succeeds() {
        let _h = crate::server::registry::tests::HOME_LOCK.lock().unwrap();
        let _g = ENV_LOCK.lock().unwrap();
        let home = std::env::temp_dir().join(format!("shunt-scp-home-{}", std::process::id()));
        std::env::set_var("SHUNT_HOME", &home);
        let dir = std::env::temp_dir().join(format!("shunt-fakescp-out-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let count = dir.join("count");
        fake_scp(&format!("n=$(cat {0} 2>/dev/null || echo 0); n=$((n+1)); echo $n > {0}; [ $n -lt 3 ] && exit 255; exit 0", count.display()));
        let local = dir.join("bin");
        std::fs::write(&local, "x").unwrap();
        SshTransport.copy_to(&ssh_client(), &local, "/remote/shunt").unwrap();
        assert_eq!(std::fs::read_to_string(&count).unwrap().trim(), "3", "two 255 flaps retried inside the call");
        std::env::remove_var("SHUNT_HOME");
        restore_path();
    }

    #[test]
    fn scp_passes_serveralive() {
        let _h = crate::server::registry::tests::HOME_LOCK.lock().unwrap();
        let _g = ENV_LOCK.lock().unwrap();
        let home = std::env::temp_dir().join(format!("shunt-scp-home4-{}", std::process::id()));
        std::env::set_var("SHUNT_HOME", &home);
        let dir = std::env::temp_dir().join(format!("shunt-fakescp-out4-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let argfile = dir.join("argv");
        std::env::set_var("FAKE_ARGV", &argfile);
        fake_scp("printf '%s\\n' \"$@\" > \"$FAKE_ARGV\"; exit 0");
        let local = dir.join("bin");
        std::fs::write(&local, "x").unwrap();
        SshTransport.copy_to(&ssh_client(), &local, "/remote/shunt").unwrap();
        let argv = std::fs::read_to_string(&argfile).unwrap();
        assert!(argv.contains("ServerAliveInterval=15"), "{argv}");
        assert!(argv.contains("ServerAliveCountMax=2"), "{argv}");
        std::env::remove_var("FAKE_ARGV");
        std::env::remove_var("SHUNT_HOME");
        restore_path();
    }

    #[test]
    fn scp_remote_path_is_shell_quoted() {
        let _h = crate::server::registry::tests::HOME_LOCK.lock().unwrap();
        let _g = ENV_LOCK.lock().unwrap();
        let home = std::env::temp_dir().join(format!("shunt-scp-home3-{}", std::process::id()));
        std::env::set_var("SHUNT_HOME", &home);
        let dir = std::env::temp_dir().join(format!("shunt-fakescp-out3-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let argfile = dir.join("lastarg");
        std::env::set_var("FAKE_ARGV", &argfile);
        fake_scp("for a in \"$@\"; do last=$a; done; printf %s \"$last\" > \"$FAKE_ARGV\"; exit 0");
        let local = dir.join("bin");
        std::fs::write(&local, "x").unwrap();
        // A qroot-derived path with spaces and `$` must reach scp
        // quoted — the remote shell would otherwise split/expand it.
        SshTransport.copy_to(&ssh_client(), &local, "/root/my dir/$(id)").unwrap();
        assert_eq!(std::fs::read_to_string(&argfile).unwrap(), "u@127.0.0.1:'/root/my dir/$(id)'");
        std::env::remove_var("FAKE_ARGV");
        std::env::remove_var("SHUNT_HOME");
        restore_path();
    }

    #[test]
    fn scp_deadline_bails_without_retry() {
        let _h = crate::server::registry::tests::HOME_LOCK.lock().unwrap();
        let _g = ENV_LOCK.lock().unwrap();
        let home = std::env::temp_dir().join(format!("shunt-scp-home2-{}", std::process::id()));
        std::env::set_var("SHUNT_HOME", &home);
        std::env::set_var("SHUNT_SSH_EXEC_TIMEOUT_S", "1");
        let dir = std::env::temp_dir().join(format!("shunt-fakescp-out2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let count = dir.join("count");
        fake_scp(&format!("n=$(cat {0} 2>/dev/null || echo 0); echo $((n+1)) > {0}; sleep 30", count.display()));
        let local = dir.join("bin");
        std::fs::write(&local, "x").unwrap();
        let err = SshTransport.copy_to(&ssh_client(), &local, "/remote/shunt").unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
        assert_eq!(std::fs::read_to_string(&count).unwrap().trim(), "1", "a hang is not a flap: no retry");
        std::env::remove_var("SHUNT_SSH_EXEC_TIMEOUT_S");
        std::env::remove_var("SHUNT_HOME");
        restore_path();
    }
}
