//! `Transport::exec` for the real ssh transport: remote argv over
//! `ssh <base_args> -o ServerAlive… <target> '<quoted argv>'` —
//! rc-255 retry, the §4.1 pre_connect hook, wire-error surfacing,
//! and a hard deadline that treats a hang as a fault, not a flap.

use super::{unreachable_err, PreConnect, SshTransport, RETRY};
use crate::server::registry::{self, ClientEntry};
use crate::server::transport::{exec_timeout_s, join_argv, wait_deadline};
use anyhow::{Context, Result};
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

pub(super) fn exec(client: &ClientEntry, argv: &[String], stdin: Option<&str>) -> Result<String> {
    let mut cmd = Command::new("ssh");
    cmd.args(SshTransport::base_args(client)?)
        // Half-open TCP detection — not in base_args because the
        // tunnel sets its own ServerAlive policy. scp spells the
        // same opts via copy_to below (design.md §5).
        .args(["-o", "ServerAliveInterval=15", "-o", "ServerAliveCountMax=2"])
        .arg(SshTransport::target(client))
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
    //
    // pre_connect (§4.1): a call that starts on a nonzero
    // conn-fail streak already knows the endpoint is down — the
    // hook runs BEFORE the first attempt rather than after
    // another wasted ConnectTimeout. Otherwise it fires after
    // the first rc-255, once per call; its outcome never gates
    // the retries but rides the final unreachable error.
    let mut hook = PreConnect::new(client);
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
            registry::conn_fail_reset(&client.name);
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
            hook.on_255(client);
            if attempt == RETRY {
                return Err(unreachable_err(client, &stderr, hook.note()));
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
