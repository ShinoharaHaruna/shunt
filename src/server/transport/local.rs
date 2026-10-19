//! Loopback transport (design.md §5): the dev machine as a client.

use super::shunt_bin;
use crate::server::registry::ClientEntry;
use anyhow::{Context, Result};
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

/// Loopback transport (design.md §5): the dev machine as a client.
/// Wire argv arrives with argv[0] = `<qroot>/bin/shunt`; locally the
/// running binary is the source of truth, so it is substituted with
/// `self.bin`. Raw argv (probe/bootstrap's `sh`/`mkdir`/`chmod` calls)
/// runs as-is. No network, no key, no remote daemon required.
pub struct LocalTransport {
    /// Binary to exec for wire ops. Default: the running `shunt`;
    /// client rows override via `local_bin`; tests inject
    /// `env!("CARGO_BIN_EXE_shunt")` or any path.
    pub bin: std::path::PathBuf,
    /// Exec timeout (client row `timeout_s`); on expiry the child is
    /// killed and the call errors. None = `exec_timeout_s()` default
    /// ceiling — a wedged wire op must not stall the dispatcher.
    pub timeout_s: Option<u64>,
}

impl Default for LocalTransport {
    fn default() -> Self {
        Self { bin: std::env::current_exe().unwrap_or_else(|_| Path::new("shunt").to_path_buf()), timeout_s: None }
    }
}

impl super::Transport for LocalTransport {
    fn exec(&self, client: &ClientEntry, argv: &[String], stdin: Option<&str>) -> Result<String> {
        let Some(first) = argv.first() else {
            anyhow::bail!("local exec: empty argv");
        };
        let (prog, rest) = if *first == shunt_bin(client) { (self.bin.clone(), &argv[1..]) } else { (std::path::PathBuf::from(first), &argv[1..]) };
        let mut cmd = Command::new(&prog);
        cmd.args(rest);
        if stdin.is_some() {
            cmd.stdin(Stdio::piped());
        } else {
            cmd.stdin(Stdio::null());
        }
        // process_group(0): pgid == child pid so wait_deadline can
        // kill the whole group on timeout (see ssh.rs for why).
        let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0).spawn().with_context(|| format!("spawn {}", prog.display()))?;
        if let Some(body) = stdin {
            // Write from a thread: a child that never reads stdin
            // while filling stdout would otherwise deadlock the
            // write — the pipe readers only start in wait_deadline.
            // Dropping the write end gives `enqueue -` its EOF.
            let mut w = child.stdin.take().expect("stdin piped");
            let body = body.to_string();
            std::thread::spawn(move || {
                let _ = w.write_all(body.as_bytes());
            });
        }
        let out = super::wait_deadline(child, self.timeout_s.unwrap_or_else(super::exec_timeout_s), &format!("local exec {}", prog.display()))?;
        if !out.status.success() {
            // Same wire convention as the ssh transport: a wire op's
            // `{"ok":false,"error":"…"}` on stdout carries the real
            // message; raw argv prints no wire JSON and falls through.
            if let Ok(v) = crate::proto::wire::extract(&String::from_utf8_lossy(&out.stdout)) {
                if v.get("ok").and_then(|o| o.as_bool()) == Some(false) {
                    if let Some(e) = v.get("error").and_then(|e| e.as_str()) {
                        anyhow::bail!("local exec {} failed: {e}", prog.display());
                    }
                }
            }
            anyhow::bail!("local exec {} rc={}: {}", prog.display(), out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn copy_to(&self, _client: &ClientEntry, local: &Path, remote_path: &str) -> Result<()> {
        std::fs::copy(local, remote_path).with_context(|| format!("copy {} -> {}", local.display(), remote_path))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::{local_client, shunt_bin, Transport};
    use super::LocalTransport;
    use std::path::PathBuf;

    #[test]
    fn local_exec_substitutes_wire_bin() {
        // argv[0]=<qroot>/bin/shunt gets replaced by self.bin; here the
        // injected bin is /bin/cat with NO args, so it echoes stdin
        // back — proving substitution + stdin piping in one shot.
        let c = local_client();
        let t = LocalTransport { bin: PathBuf::from("/bin/cat"), timeout_s: None };
        let out = t.exec(&c, &[shunt_bin(&c)], Some("hello-wire")).unwrap();
        assert_eq!(out, "hello-wire");
    }

    #[test]
    fn local_exec_raw_argv_passthrough() {
        // bootstrap/probe call raw tools (sh/mkdir), not the wire bin.
        let c = local_client();
        let t = LocalTransport { bin: PathBuf::from("/nonexistent-shunt"), timeout_s: None };
        let out = t.exec(&c, &["sh".into(), "-c".into(), "echo raw-ok".into()], None).unwrap();
        assert_eq!(out.trim(), "raw-ok");
    }

    #[test]
    fn local_exec_drains_big_stdout_under_deadline() {
        // Same >64 KiB pipe deadlock as the ssh transport, with
        // timeout_s set so a regression hangs only briefly.
        let c = local_client();
        let t = LocalTransport { bin: PathBuf::from("/bin/sh"), timeout_s: Some(30) };
        let out = t.exec(&c, &[shunt_bin(&c), "-c".into(), "yes | head -c 200000".into()], None).unwrap();
        assert_eq!(out.len(), 200000);
    }

    #[test]
    fn local_exec_surfaces_wire_error() {
        // Wire ops fail as `{"ok":false,"error":"…"}` on stdout +
        // rc 1; the caller must see the message, not a bare rc line.
        let c = local_client();
        let t = LocalTransport { bin: PathBuf::from("/bin/sh"), timeout_s: None };
        let err = t.exec(&c, &[shunt_bin(&c), "-c".into(), "printf '%s\n' '{\"protocol\":1,\"ok\":false,\"error\":\"queue wedged\"}'; exit 1".into()], None).unwrap_err();
        assert!(err.to_string().contains("queue wedged"), "{err}");
    }

    #[test]
    fn local_exec_timeout_kills() {
        // timeout_s=0 fires immediately; the child must be killed,
        // not orphaned.
        let c = local_client();
        let t = LocalTransport { bin: PathBuf::from("/bin/sleep"), timeout_s: Some(0) };
        let err = t.exec(&c, &[shunt_bin(&c), "30".into()], None).unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
    }

    #[test]
    fn local_exec_none_timeout_uses_default_deadline() {
        // Row timeout_s=None must NOT wait forever: it falls back to
        // the shared exec ceiling (SHUNT_SSH_EXEC_TIMEOUT_S=1 here),
        // so a wedged wire op can't stall the dispatcher sweep.
        let _g = super::super::ENV_LOCK.lock().unwrap();
        std::env::set_var("SHUNT_SSH_EXEC_TIMEOUT_S", "1");
        let c = local_client();
        let t = LocalTransport { bin: PathBuf::from("/bin/sleep"), timeout_s: None };
        let err = t.exec(&c, &[shunt_bin(&c), "30".into()], None).unwrap_err();
        std::env::remove_var("SHUNT_SSH_EXEC_TIMEOUT_S");
        assert!(err.to_string().contains("timed out"), "{err}");
    }
}
