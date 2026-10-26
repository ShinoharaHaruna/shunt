//! SSH exec transport (design.md §5). All argv crossing SSH is
//! shell-quoted here; call sites never hand-roll quoting.
//!
//! Wire convention: the remote binary lives at `<qroot>/bin/shunt`
//! (installed by `client bootstrap`), so every wire argv starts with
//! `shunt_bin(client)`.

mod local;
mod ssh;

pub use local::LocalTransport;
pub(crate) use ssh::ssh_tool_cmd;
pub use ssh::SshTransport;

use crate::server::registry::{self, ClientEntry};
use anyhow::Result;
use std::os::unix::process::CommandExt;
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
pub fn ssh_conn_args(client: &ClientEntry) -> Result<Vec<String>> {
    let mut v = SshTransport::base_args(client)?;
    v.push(SshTransport::target(client));
    Ok(v)
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
    // `--root` must precede a `--` separator or it lands in the
    // positional tail itself and the qroot flag is lost — so an
    // existing `--root` only suppresses injection when it sits
    // BEFORE `--` (after `--` it is positional data, not the flag).
    let i = tail.iter().position(|s| *s == "--").unwrap_or(tail.len());
    if tail[..i].contains(&"--root") {
        v.extend(tail.iter().map(|s| s.to_string()));
        return v;
    }
    v.extend(tail[..i].iter().map(|s| s.to_string()));
    v.push("--root".into());
    v.push(client.qroot.clone());
    v.extend(tail[i..].iter().map(|s| s.to_string()));
    v
}

/// Exec one wire op on a resolved queue: `wire_argv` and the exec
/// target are the same overlay client — spelled once here so no call
/// site can pair one queue's `--root` argv with another's identity.
/// The transport is caller-supplied: tests inject fakes, and
/// `transport_for` stays a per-client caller decision.
pub fn queue_exec(t: &dyn Transport, queue: &crate::server::registry::ResolvedQueue, tail: &[&str], stdin: Option<&str>) -> Result<String> {
    t.exec(&queue.client, &wire_argv(&queue.client, tail), stdin)
}

/// Hard deadline for one wire op. `ConnectTimeout` only bounds the
/// ssh handshake; a live-but-hung remote command (frozen NFS qroot,
/// remote OOM) — or a wedged loopback op — would otherwise block the
/// caller forever. ServerAlive catches dead TCP in ~30s; this
/// ceiling catches the rest. Wire ops are short (status/enqueue/
/// push); `logs -f` never crosses the transport. A row's `timeout_s`
/// overrides; `SHUNT_SSH_EXEC_TIMEOUT_S` overrides that (tests).
const EXEC_TIMEOUT_S: u64 = 120;

pub(crate) fn exec_timeout_s() -> u64 {
    std::env::var("SHUNT_SSH_EXEC_TIMEOUT_S").ok().and_then(|s| s.parse().ok()).filter(|&n| n > 0).unwrap_or(EXEC_TIMEOUT_S)
}

/// Poll a spawned child to completion or kill it at the deadline.
/// Shared by the ssh transport (exec + scp hard ceiling) and the
/// loopback transport (row `timeout_s` else `exec_timeout_s()`).
///
/// stdout/stderr drain on reader threads from the start: a child
/// that fills its pipe buffer (~64 KiB) otherwise blocks on write
/// and never exits, so a verbose remote op (`logs -n 200`, noisy
/// `client exec`) would deadlock against the deadline — or forever
/// where the caller polls without one. Spawn sites must set
/// `process_group(0)` so the deadline can kill the whole group
/// (pgid == child pid): a bare `kill()` would orphan grandchildren
/// still holding the pipes, and the drain below could hang.
///
/// Reader threads signal pipe EOF over a channel and drain into a
/// shared buffer: an in-group (or detached) process inheriting a
/// pipe write end can outlive the child, so on the exit path the
/// drain wait is bounded by the same deadline. If the bounded wait
/// expires we return the bytes collected so far with the child's
/// real status — the call DID succeed, so partial output beats an
/// error that misreports it as a timeout.
pub(crate) fn wait_deadline(mut child: std::process::Child, timeout_s: u64, what: &str) -> Result<std::process::Output> {
    use std::io::Read;
    use std::sync::{mpsc, Arc, Mutex};
    type Buf = Arc<Mutex<Vec<u8>>>;
    fn drain(mut pipe: impl Read + Send + 'static, buf: Buf) -> mpsc::Receiver<()> {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            while let Ok(n) = pipe.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                if let Ok(mut b) = buf.lock() {
                    b.extend_from_slice(&chunk[..n]);
                }
            }
            let _ = tx.send(());
        });
        rx
    }
    let out_buf: Buf = Arc::new(Mutex::new(Vec::new()));
    let err_buf: Buf = Arc::new(Mutex::new(Vec::new()));
    let out_rx = drain(child.stdout.take().expect("stdout piped"), out_buf.clone());
    let err_rx = drain(child.stderr.take().expect("stderr piped"), err_buf.clone());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_s);
    let take = |b: &Buf| b.lock().map(|mut g| std::mem::take(&mut *g)).unwrap_or_default();
    loop {
        match child.try_wait()? {
            Some(status) => {
                // Wait for pipe EOF, bounded by the same deadline: a
                // pipe holder outliving the child must not stall the
                // return past it. Reader handles were never kept —
                // the threads end at EOF regardless.
                let _ = out_rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()));
                let _ = err_rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()));
                return Ok(std::process::Output { status, stdout: take(&out_buf), stderr: take(&err_buf) });
            }
            None if std::time::Instant::now() >= deadline => {
                // Group kill first (grandchildren included), the
                // direct child as a belt. The receivers drop and
                // the reader threads end at pipe EOF: an
                // out-of-group pipe holder must not delay the bail.
                let _ = crate::client::exec::signal_pgid(child.id(), "KILL");
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("{what} timed out after {timeout_s}s");
            }
            None => std::thread::sleep(std::time::Duration::from_millis(50)),
        }
    }
}

/// The `pre_connect` hook's own deadline (design.md §4.1): a
/// bring-up legitimately blocks until connected, so it gets a
/// separate budget rather than sharing the wire-op ceiling.
/// `SHUNT_PRE_CONNECT_TIMEOUT_S` overrides (tests).
const PRE_CONNECT_TIMEOUT_S: u64 = 60;

fn pre_connect_timeout_s() -> u64 {
    std::env::var("SHUNT_PRE_CONNECT_TIMEOUT_S").ok().and_then(|s| s.parse().ok()).filter(|&n| n > 0).unwrap_or(PRE_CONNECT_TIMEOUT_S)
}

/// Run the client's `pre_connect` reachability hook on the dev
/// machine (design.md §4.1): `sh -c`, stdin null, ambient env
/// inherited, killed as a process group at its deadline. The result
/// never gates the caller's retries — this returns a short outcome
/// note for the unreachable error (`rc=1: <stderr tail>` / `timed
/// out`), or None when no hook is configured. Callers fire it at
/// most once per call: after the first rc-255, or before the first
/// attempt when the conn-fail streak is already nonzero.
pub(crate) fn run_pre_connect(client: &ClientEntry) -> Option<String> {
    let snippet = client.pre_connect.trim();
    if snippet.is_empty() {
        return None;
    }
    // process_group(0) so the deadline kill takes grandchildren too
    // — same discipline as every other spawned child here.
    let child = std::process::Command::new("sh").arg("-c").arg(snippet).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).process_group(0).spawn();
    let out = match child {
        Ok(c) => match wait_deadline(c, pre_connect_timeout_s(), "pre_connect") {
            Ok(o) => o,
            Err(e) => return Some(format!("{e:#}")),
        },
        Err(e) => return Some(format!("spawn failed: {e:#}")),
    };
    let rc = out.status.code().unwrap_or(-1);
    // Tail: last non-empty stderr lines on one line, capped — the
    // hook's complaint rides the unreachable error verbatim.
    let tail = {
        let s = String::from_utf8_lossy(&out.stderr);
        let joined = s.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" | ");
        const CAP: usize = 240;
        if joined.chars().count() > CAP {
            format!("…{}", joined.chars().skip(joined.chars().count() - CAP).collect::<String>())
        } else {
            joined
        }
    };
    Some(if tail.is_empty() { format!("rc={rc}") } else { format!("rc={rc}: {tail}") })
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

/// `load_client` + `transport_for`: the pairing every "one op on a
/// named client" call site spells out. Callers that need only the
/// row, a resolved queue's execution view (`ResolvedQueue.client`),
/// a mutated entry, or an injected fake still load and pick
/// separately — this is the common-case spelling, not the only one.
pub fn client_transport(name: &str) -> Result<(ClientEntry, Box<dyn Transport>)> {
    let client = registry::load_client(name)?;
    let t = transport_for(&client);
    Ok((client, t))
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

/// Serializes tests that mutate process env vars
/// (`SHUNT_SSH_EXEC_TIMEOUT_S`, PATH) across transport modules.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
fn local_client() -> ClientEntry {
    use crate::server::registry::SshEndpoint;
    use std::collections::BTreeMap;
    ClientEntry { name: "localhost".into(), local: true, disabled: false, local_bin: None, bin_path: None, queues: Default::default(), default_queue: None, timeout_s: None, ssh_timeout_s: None, pre_connect: String::new(), shutdown_cmd: None, expire: String::new(), env_setup: String::new(), shell: String::new(), endpoint_rotates: false, shared: false, exit_on_idle: false, ssh: SshEndpoint::default(), qroot: "/tmp/qt".into(), deploy_root: "/tmp".into(), facts: BTreeMap::new(), billing: None }
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

    /// A process-group member (or detached grandchild) that keeps
    /// a pipe write end open after the child exits must not stall
    /// the return: the drain wait is bounded by the deadline and
    /// yields the bytes collected so far.
    #[test]
    fn pipe_holder_outliving_child_bounds_drain() {
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};
        let mut c = Command::new("sh");
        // The backgrounded sleep inherits stdout; `sh` exits at once
        // while the pipe stays open for the sleep's full run.
        let child = c.arg("-c").arg("sleep 10 & echo hi").stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0).spawn().unwrap();
        let t0 = std::time::Instant::now();
        let out = wait_deadline(child, 1, "pipe-holder test").unwrap();
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hi");
        assert!(t0.elapsed() < std::time::Duration::from_secs(8), "drain outlived the deadline");
    }

    #[test]
    fn wire_argv_root_after_separator_is_positional() {
        let c = super::local_client();
        // `--root` behind `--` is positional data, not the qroot flag
        // — injection must still happen, ahead of the separator.
        let v = wire_argv(&c, &["enqueue", "--", "-", "--root"]);
        assert_eq!(v, vec!["/tmp/qt/bin/shunt", "enqueue", "--root", "/tmp/qt", "--", "-", "--root"]);
    }
}
