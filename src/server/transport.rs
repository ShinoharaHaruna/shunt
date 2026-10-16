//! SSH exec transport (design.md §5). All argv crossing SSH is
//! shell-quoted here; call sites never hand-roll quoting.
//!
//! Wire convention: the remote binary lives at `<qroot>/bin/shunt`
//! (installed by `client bootstrap`), so every wire argv starts with
//! `shunt_bin(client)`.

use crate::server::registry::ClientEntry;
use anyhow::{Context, Result};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// Quote one arg for the remote shell (`%q` equivalent).
pub fn shell_quote(arg: &str) -> String {
    if arg.chars().all(|c| {
        c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | '/' | ':' | '=' | '+' | ',')
    }) && !arg.is_empty()
    {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', "'\\''"))
}

/// Join argv into one remote shell command string.
pub fn join_argv(argv: &[String]) -> String {
    argv.iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Path of the remote `shunt` binary on a client. The dispatcher and
/// every wire op prefix argv with this; bootstrap installs to it.
pub fn shunt_bin(client: &ClientEntry) -> String {
    format!("{}/bin/shunt", client.qroot.trim_end_matches('/'))
}

/// Full wire argv: `<qroot>/bin/shunt <tail> --root <qroot>`. The
/// registry's qroot is authoritative — the remote never resolves its
/// own queue root. Tail args already carrying `--root` are respected.
pub fn wire_argv(client: &ClientEntry, tail: &[&str]) -> Vec<String> {
    let mut v = vec![shunt_bin(client)];
    v.extend(tail.iter().map(|s| s.to_string()));
    if !tail.contains(&"--root") {
        v.push("--root".into());
        v.push(client.qroot.clone());
    }
    v
}

/// Pick the transport for a client row (design.md §5): local loopback
/// for `local = true` rows (incl. the built-in `localhost`), SSH else.
pub fn transport_for(client: &ClientEntry) -> Box<dyn Transport> {
    if client.local || client.name == "localhost" {
        Box::new(LocalTransport::default())
    } else {
        Box::new(SshTransport)
    }
}

/// Exec transport over SSH. Tests substitute a fake (canned output or
/// a locally-built binary on a tmp qroot).
pub trait Transport {
    /// Run `argv` on the client, piping `stdin` if given. Returns
    /// remote stdout. Err covers both "ssh could not connect" and a
    /// nonzero remote exit; callers that need to distinguish run a
    /// cheap probe (`status --json`) after a failure.
    fn exec(&self, client: &ClientEntry, argv: &[String], stdin: Option<&str>) -> Result<String>;

    /// Copy a local file to `remote_path` on the client (bootstrap).
    fn copy_to(&self, client: &ClientEntry, local: &Path, remote_path: &str) -> Result<()>;
}

/// Real transport: `ssh -p PORT -o BatchMode=yes -o ConnectTimeout=15
/// -i KEY USER@HOST '<quoted argv>'`.
pub struct SshTransport;

impl SshTransport {
    fn base_args(client: &ClientEntry) -> Vec<String> {
        let mut v = vec![
            "-p".into(),
            client.ssh.port.to_string(),
            "-o".into(),
            "BatchMode=yes".into(),
            "-o".into(),
            "ConnectTimeout=15".into(),
        ];
        if !client.ssh.key.is_empty() {
            v.push("-i".into());
            v.push(client.ssh.key.clone());
        }
        v.push(format!("{}@{}", client.ssh.user, client.ssh.host));
        v
    }
}

impl Transport for SshTransport {
    fn exec(&self, client: &ClientEntry, argv: &[String], stdin: Option<&str>) -> Result<String> {
        let mut cmd = Command::new("ssh");
        cmd.args(Self::base_args(client)).arg(join_argv(argv));
        if stdin.is_some() {
            cmd.stdin(Stdio::piped());
        } else {
            cmd.stdin(Stdio::null());
        }
        let mut child = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawn ssh")?;
        if let Some(body) = stdin {
            child
                .stdin
                .as_mut()
                .expect("stdin piped")
                .write_all(body.as_bytes())?;
        }
        let out = child.wait_with_output().context("wait ssh")?;
        if !out.status.success() {
            anyhow::bail!(
                "ssh {}@{}:{} rc={}: {}",
                client.ssh.user,
                client.ssh.host,
                client.ssh.port,
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn copy_to(&self, client: &ClientEntry, local: &Path, remote_path: &str) -> Result<()> {
        // keep
        // scp, matching the ssh flags; -P for port.
        let mut args = vec![
            "-P".into(),
            client.ssh.port.to_string(),
            "-o".into(),
            "BatchMode=yes".into(),
            "-o".into(),
            "ConnectTimeout=15".into(),
        ];
        if !client.ssh.key.is_empty() {
            args.push("-i".into());
            args.push(client.ssh.key.clone());
        }
        args.push(local.display().to_string());
        args.push(format!(
            "{}@{}:{}",
            client.ssh.user, client.ssh.host, remote_path
        ));
        let out = Command::new("scp")
            .args(&args)
            .stdin(Stdio::null())
            .output()
            .context("spawn scp")?;
        if !out.status.success() {
            anyhow::bail!(
                "scp to {} failed: {}",
                client.name,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }
}

/// Loopback transport (design.md §5): the dev machine as a client.
/// Wire argv arrives with argv[0] = `<qroot>/bin/shunt`; locally the
/// running binary is the source of truth, so it is substituted with
/// `self.bin`. Raw argv (probe/bootstrap's `sh`/`mkdir`/`chmod` calls)
/// runs as-is. No network, no key, no remote daemon required.
pub struct LocalTransport {
    /// Binary to exec for wire ops. Default: the running `shunt`;
    /// tests inject `env!("CARGO_BIN_EXE_shunt")` or any path.
    pub bin: std::path::PathBuf,
}

impl Default for LocalTransport {
    fn default() -> Self {
        Self {
            bin: std::env::current_exe().unwrap_or_else(|_| Path::new("shunt").to_path_buf()),
        }
    }
}

impl Transport for LocalTransport {
    fn exec(&self, client: &ClientEntry, argv: &[String], stdin: Option<&str>) -> Result<String> {
        let Some(first) = argv.first() else {
            anyhow::bail!("local exec: empty argv");
        };
        let (prog, rest) = if *first == shunt_bin(client) {
            (self.bin.clone(), &argv[1..])
        } else {
            (std::path::PathBuf::from(first), &argv[1..])
        };
        let mut cmd = Command::new(&prog);
        cmd.args(rest);
        if stdin.is_some() {
            cmd.stdin(Stdio::piped());
        } else {
            cmd.stdin(Stdio::null());
        }
        let mut child = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("spawn {}", prog.display()))?;
        if let Some(body) = stdin {
            child
                .stdin
                .as_mut()
                .expect("stdin piped")
                .write_all(body.as_bytes())?;
        }
        let out = child.wait_with_output().context("wait local exec")?;
        if !out.status.success() {
            anyhow::bail!(
                "local exec {} rc={}: {}",
                prog.display(),
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn copy_to(&self, _client: &ClientEntry, local: &Path, remote_path: &str) -> Result<()> {
        std::fs::copy(local, remote_path)
            .with_context(|| format!("copy {} -> {}", local.display(), remote_path))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::registry::{ClientEntry, SshEndpoint};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn local_client() -> ClientEntry {
        ClientEntry {
            name: "localhost".into(),
            local: true,
            ssh: SshEndpoint::default(),
            qroot: "/tmp/qt".into(),
            deploy_root: "/tmp".into(),
            facts: BTreeMap::new(),
            billing: None,
        }
    }

    #[test]
    fn quote_safe_passthrough() {
        assert_eq!(shell_quote("runs/x.json"), "runs/x.json");
    }

    #[test]
    fn quote_spaces_and_quotes() {
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn wire_argv_appends_root_once() {
        let c = local_client();
        let v = wire_argv(&c, &["status", "--json"]);
        assert_eq!(
            v,
            vec!["/tmp/qt/bin/shunt", "status", "--json", "--root", "/tmp/qt"]
        );
        let v2 = wire_argv(&c, &["init", "--root", "/other"]);
        assert_eq!(v2.iter().filter(|a| *a == "--root").count(), 1);
    }

    #[test]
    fn local_exec_substitutes_wire_bin() {
        // argv[0]=<qroot>/bin/shunt gets replaced by self.bin; here the
        // injected bin is /bin/cat with NO args, so it echoes stdin
        // back — proving substitution + stdin piping in one shot.
        let c = local_client();
        let t = LocalTransport {
            bin: PathBuf::from("/bin/cat"),
        };
        let out = t.exec(&c, &[shunt_bin(&c)], Some("hello-wire")).unwrap();
        assert_eq!(out, "hello-wire");
    }

    #[test]
    fn local_exec_raw_argv_passthrough() {
        // bootstrap/probe call raw tools (sh/mkdir), not the wire bin.
        let c = local_client();
        let t = LocalTransport {
            bin: PathBuf::from("/nonexistent-shunt"),
        };
        let out = t
            .exec(&c, &["sh".into(), "-c".into(), "echo raw-ok".into()], None)
            .unwrap();
        assert_eq!(out.trim(), "raw-ok");
    }
}
