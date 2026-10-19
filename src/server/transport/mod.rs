//! SSH exec transport (design.md §5). All argv crossing SSH is
//! shell-quoted here; call sites never hand-roll quoting.
//!
//! Wire convention: the remote binary lives at `<qroot>/bin/shunt`
//! (installed by `client bootstrap`), so every wire argv starts with
//! `shunt_bin(client)`.

mod local;
mod ssh;

pub use local::LocalTransport;
pub use ssh::SshTransport;

use crate::server::registry::ClientEntry;
use anyhow::{Context, Result};
use std::path::Path;

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
    if tail.contains(&"--root") {
        v.extend(tail.iter().map(|s| s.to_string()));
        return v;
    }
    // `--root` must precede a `--` separator or it lands in the
    // positional tail itself and the qroot flag is lost.
    let i = tail.iter().position(|s| *s == "--").unwrap_or(tail.len());
    v.extend(tail[..i].iter().map(|s| s.to_string()));
    v.push("--root".into());
    v.push(client.qroot.clone());
    v.extend(tail[i..].iter().map(|s| s.to_string()));
    v
}

/// Poll a spawned child to completion or kill it at the deadline.
/// Shared by the ssh transport (exec + scp hard ceiling) and the
/// loopback transport's row-configured `timeout_s`.
pub(crate) fn wait_deadline(mut child: std::process::Child, timeout_s: u64, what: &str) -> Result<std::process::Output> {
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

/// ControlMaster socket dir: `$SHUNT_HOME/cm/` — shunt-owned mux,
/// never the user's ~/.ssh/config (design.md §4.1).
pub fn cm_dir() -> Result<std::path::PathBuf> {
    Ok(crate::server::registry::shunt_home()?.join("cm"))
}

#[cfg(test)]
fn local_client() -> ClientEntry {
    use crate::server::registry::SshEndpoint;
    use std::collections::BTreeMap;
    ClientEntry { name: "localhost".into(), local: true, disabled: false, local_bin: None, timeout_s: None, ssh_timeout_s: None, shutdown_cmd: None, expire: String::new(), env_setup: String::new(), shell: String::new(), endpoint_rotates: false, ssh: SshEndpoint::default(), qroot: "/tmp/qt".into(), deploy_root: "/tmp".into(), facts: BTreeMap::new(), billing: None }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let c = super::local_client();
        let v = wire_argv(&c, &["status", "--json"]);
        assert_eq!(v, vec!["/tmp/qt/bin/shunt", "status", "--json", "--root", "/tmp/qt"]);
        let v2 = wire_argv(&c, &["init", "--root", "/other"]);
        assert_eq!(v2.iter().filter(|a| *a == "--root").count(), 1);
    }
}
