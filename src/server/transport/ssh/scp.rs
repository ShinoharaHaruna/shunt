//! `Transport::copy_to` for the real ssh transport: legacy
//! `scp -O` riding the same base flags and ControlMaster mux as
//! exec, with the same rc-255 retry/pre_connect discipline —
//! bootstrap pushes the binary through here, so one
//! `Exceeded MaxStartups` flap must not fail the copy.

use super::{unreachable_err, PreConnect, SshTransport, RETRY};
use crate::server::registry::{self, ClientEntry};
use crate::server::transport::{exec_timeout_s, shell_quote, wait_deadline};
use anyhow::{Context, Result};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

pub(super) fn copy_to(client: &ClientEntry, local: &Path, remote_path: &str) -> Result<()> {
    // scp shares the ssh flags — including the ControlMaster mux,
    // so bulk copies ride the same single connection (design.md
    // §4.1). -P for port (scp) vs -p (ssh): reuse base_args but
    // drop its leading `-p PORT` pair and trailing `user@host`
    // (scp sources are paths; a bare user@host reads as a local file).
    // base_args is flags-only now: drop its leading `-p PORT`
    // (scp spells it -P), then local source + user@host:path.
    let mut args = vec!["-O".into(), "-P".into(), client.ssh.port.to_string()];
    args.extend(SshTransport::base_args(client)?.into_iter().skip(2));
    // Same double bound as exec (design.md §5): half-open TCP
    // dies in ~30s instead of riding the full hard deadline.
    args.extend(["-o".into(), "ServerAliveInterval=15".into(), "-o".into(), "ServerAliveCountMax=2".into()]);
    args.push(local.display().to_string());
    // Explicit legacy SCP (-O) keeps older remote scp peers usable
    // and makes shell quoting correct regardless of the local
    // default: SFTP would preserve these quotes literally. The
    // legacy remote shell needs them to prevent splitting/injection.
    args.push(format!("{}@{}:{}", client.ssh.user, client.ssh.host, shell_quote(remote_path)));
    // Same contract as exec: retry rc-255 flaps, bounded by the
    // hard deadline (a hang is not a flap — bail without retry or
    // streak bump), feed the drift counter only on exhaustion.
    // Bootstrap pushes the binary through here, so a single
    // `Exceeded MaxStartups` flap must not fail the copy.
    // pre_connect fires identically to exec: eager on a nonzero
    // streak, else after the first rc-255, once per call.
    let mut hook = PreConnect::new(client);
    for attempt in 1..=RETRY {
        let child = Command::new("scp").args(&args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0).spawn().context("spawn scp")?;
        let out = wait_deadline(child, exec_timeout_s(), "scp")?;
        if out.status.success() {
            registry::conn_fail_reset(&client.name);
            return Ok(());
        }
        let rc = out.status.code().unwrap_or(-1);
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if rc == 255 {
            hook.on_255(client);
        }
        if rc != 255 || attempt == RETRY {
            if rc == 255 {
                return Err(unreachable_err(client, &stderr, hook.note()));
            }
            anyhow::bail!("scp to {} failed rc={}: {}", client.name, rc, stderr);
        }
        std::thread::sleep(std::time::Duration::from_millis(1500));
    }
    unreachable!("retry loop always returns");
}
