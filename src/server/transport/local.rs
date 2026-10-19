//! Loopback transport (design.md §5): the dev machine as a client.

use super::shunt_bin;
use crate::server::registry::ClientEntry;
use anyhow::{Context, Result};
use std::io::Write;
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
    /// killed and the call errors. None = wait indefinitely.
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
        let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().with_context(|| format!("spawn {}", prog.display()))?;
        if let Some(body) = stdin {
            child.stdin.as_mut().expect("stdin piped").write_all(body.as_bytes())?;
        }
        // Close stdin so `enqueue -` sees EOF; the timeout path polls
        // try_wait and never drops the pipe otherwise.
        drop(child.stdin.take());
        let out = match self.timeout_s {
            Some(t) => super::wait_deadline(child, t, &format!("local exec {}", prog.display()))?,
            None => child.wait_with_output().context("wait local exec")?,
        };
        if !out.status.success() {
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
    fn local_exec_timeout_kills() {
        // timeout_s=0 fires immediately; the child must be killed,
        // not orphaned.
        let c = local_client();
        let t = LocalTransport { bin: PathBuf::from("/bin/sleep"), timeout_s: Some(0) };
        let err = t.exec(&c, &[shunt_bin(&c), "30".into()], None).unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
    }
}
