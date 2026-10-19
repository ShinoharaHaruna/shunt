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
    if arg.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | '/' | ':' | '=' | '+' | ',')) && !arg.is_empty() {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', "'\\''"))
}

/// ssh connection args for ad-hoc uses (tunnel manager): the same
/// flags exec/scp use, without a remote command appended.
pub fn ssh_conn_args(client: &ClientEntry) -> Vec<String> {
    let mut v = SshTransport::base_args(client);
    v.push(SshTransport::target(client));
    v
}

/// Join argv into one remote shell command string.
pub fn join_argv(argv: &[String]) -> String {
    argv.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ")
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
        let mut t = LocalTransport::default();
        if let Some(b) = &client.local_bin {
            t.bin = std::path::PathBuf::from(b);
        }
        t.timeout_s = client.timeout_s;
        Box::new(t)
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

/// ControlMaster socket dir: `$SHUNT_HOME/cm/` — shunt-owned mux,
/// never the user's ~/.ssh/config (design.md §4.1).
pub fn cm_dir() -> Result<std::path::PathBuf> {
    Ok(crate::server::registry::shunt_home()?.join("cm"))
}

fn cm_sock(client: &ClientEntry) -> String {
    cm_dir().map(|d| d.join(format!("{}.sock", client.name)).display().to_string()).unwrap_or_else(|_| "/dev/null".into())
}

impl SshTransport {
    /// ssh connection flags (-p, BatchMode, timeouts, TOFU known_hosts,
    /// key, user@host, mux). Shared by exec, scp, and the tunnel
    /// manager.
    pub(crate) fn base_args(client: &ClientEntry) -> Vec<String> {
        let kh = crate::server::registry::known_hosts_path().map(|p| p.display().to_string()).unwrap_or_else(|_| "/dev/null".into());
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

impl Transport for SshTransport {
    fn exec(&self, client: &ClientEntry, argv: &[String], stdin: Option<&str>) -> Result<String> {
        let mut cmd = Command::new("ssh");
        cmd.args(Self::base_args(client)).arg(Self::target(client)).arg(join_argv(argv));
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
            let out = child.wait_with_output().context("wait ssh")?;
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

impl Transport for LocalTransport {
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
            Some(t) => {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(t);
                loop {
                    match child.try_wait()? {
                        Some(_) => break child.wait_with_output().context("wait local exec")?,
                        None if std::time::Instant::now() >= deadline => {
                            let _ = child.kill();
                            let _ = child.wait_with_output();
                            anyhow::bail!("local exec {} timed out after {}s", prog.display(), t);
                        }
                        None => std::thread::sleep(std::time::Duration::from_millis(50)),
                    }
                }
            }
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
    use super::*;
    use crate::server::registry::{ClientEntry, SshEndpoint};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn local_client() -> ClientEntry {
        ClientEntry { name: "localhost".into(), local: true, disabled: false, local_bin: None, timeout_s: None, ssh_timeout_s: None, shutdown_cmd: None, expire: String::new(), env_setup: String::new(), shell: String::new(), endpoint_rotates: false, ssh: SshEndpoint::default(), qroot: "/tmp/qt".into(), deploy_root: "/tmp".into(), facts: BTreeMap::new(), billing: None }
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
        assert_eq!(v, vec!["/tmp/qt/bin/shunt", "status", "--json", "--root", "/tmp/qt"]);
        let v2 = wire_argv(&c, &["init", "--root", "/other"]);
        assert_eq!(v2.iter().filter(|a| *a == "--root").count(), 1);
    }

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
