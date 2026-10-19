//! Box-side SOCKS5 proxy via SSH reverse dynamic forward
//! (design.md §4.4): `ssh -N -R <port>` opens `127.0.0.1:<port>` as a
//! SOCKS5 listener ON THE BOX, tunnelled back through the dev
//! machine's network — the escape hatch for boxes whose outbound
//! connectivity is poor (e.g. mainland-China GPU vendors).
//!
//! `up` also writes `ALL_PROXY=socks5h://127.0.0.1:<port>` into the
//! queue's `env` file so tasks pick the proxy up transparently (the
//! runner strips ambient proxy vars, then injects the env file, so a
//! file-set proxy wins). `down` removes both.
//!
//! State under `$SHUNT_HOME/tunnels/`: `<name>.pid`, `<name>.port`,
//! `<name>.log`. Liveness is pidfile + `/proc` — same discipline as
//! the client daemon.

use crate::server::registry::{self, ClientEntry};
use crate::server::transport::{ssh_conn_args, Transport};
use anyhow::{Context, Result};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const DEFAULT_SOCKS_PORT: u16 = 1080;

fn tunnels_dir() -> Result<PathBuf> {
    Ok(registry::shunt_home()?.join("tunnels"))
}

fn pid_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.pid"))
}

fn port_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.port"))
}

/// Upsert or remove one KEY=value line in `<qroot>/env` on the box,
/// over the client's transport (works for ssh and loopback alike).
fn env_write(t: &dyn Transport, client: &ClientEntry, key: &str, value: Option<&str>) -> Result<()> {
    const SCRIPT: &str = r#"
f="$1/env"; k="$2"; v="$3"
mkdir -p "$1"; touch "$f"
grep -v "^$k=" "$f" > "$f.tmp" 2>/dev/null || true
[ -n "$v" ] && echo "$k=$v" >> "$f.tmp"
mv "$f.tmp" "$f"
"#;
    let argv = ["sh".to_string(), "-c".to_string(), SCRIPT.to_string(), "sh".to_string(), client.qroot.clone(), key.to_string(), value.unwrap_or("").to_string()];
    t.exec(client, &argv, None)?;
    Ok(())
}

/// Reported tunnel state.
pub struct TunnelState {
    pub alive: bool,
    pub port: Option<u16>,
    /// Pid is dead but state files remain: the box's env may still
    /// carry ALL_PROXY pointing at a dead proxy (poisoned).
    pub stale: bool,
}

fn read_pid(dir: &Path, name: &str) -> Option<u32> {
    let s = std::fs::read_to_string(pid_path(dir, name)).ok()?;
    s.trim().parse().ok()
}

fn pid_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// A tunnel was set up for `name` at some point: a pid or port
/// state file exists, so the box's env may still carry ALL_PROXY
/// even after the ssh process died (endpoint drift, reboot).
fn recorded_in(dir: &Path, name: &str) -> bool {
    pid_path(dir, name).exists() || port_path(dir, name).exists()
}

/// `recorded_in` against the real `$SHUNT_HOME/tunnels/`.
pub fn recorded(name: &str) -> Result<bool> {
    Ok(recorded_in(&tunnels_dir()?, name))
}

fn status_in(dir: &Path, name: &str) -> TunnelState {
    let port = std::fs::read_to_string(port_path(dir, name)).ok().and_then(|s| s.trim().parse::<u16>().ok());
    let alive = matches!(read_pid(dir, name), Some(pid) if pid_alive(pid));
    let stale = !alive && recorded_in(dir, name);
    TunnelState { alive, port, stale }
}

/// Current tunnel state for a client (no network needed).
pub fn status(name: &str) -> Result<TunnelState> {
    Ok(status_in(&tunnels_dir()?, name))
}

/// List every recorded tunnel name (clients/tunnels dir).
pub fn list() -> Result<Vec<String>> {
    let dir = tunnels_dir()?;
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for e in std::fs::read_dir(&dir)? {
        let e = e?;
        let name = e.file_name().to_string_lossy().into_owned();
        if let Some(n) = name.strip_suffix(".pid") {
            out.push(n.to_string());
        }
    }
    out.sort();
    Ok(out)
}

/// Open the tunnel: detached `ssh -N -R <port>`, then write
/// `ALL_PROXY=socks5h://127.0.0.1:<port>` into the box's queue env.
/// Returns the pid. Idempotent: an already-live tunnel is reported.
pub fn up(t: &dyn Transport, client: &ClientEntry, port: Option<u16>) -> Result<u32> {
    if client.local {
        anyhow::bail!("{} uses the loopback transport — no tunnel needed", client.name);
    }
    let dir = tunnels_dir()?;
    if let Some(pid) = read_pid(&dir, &client.name) {
        if pid_alive(pid) {
            return Ok(pid);
        }
    }
    let port = port.unwrap_or(DEFAULT_SOCKS_PORT);
    registry::private_dir(&dir)?;
    let log = std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(dir.join(format!("{}.log", client.name)))?;
    let err = log.try_clone()?;

    let mut args = vec!["ssh".to_string()];
    args.extend(ssh_conn_args(client));
    args.extend(["-N".into(), "-R".into(), port.to_string(), "-o".into(), "ExitOnForwardFailure=yes".into(), "-o".into(), "ServerAliveInterval=30".into(), "-o".into(), "ServerAliveCountMax=3".into()]);
    let mut child = Command::new("ssh").args(&args[1..]).stdin(Stdio::null()).stdout(log).stderr(err).process_group(0).spawn().context("spawn ssh tunnel")?;
    // ExitOnForwardFailure makes a bad bind exit fast; give it a beat.
    std::thread::sleep(std::time::Duration::from_millis(500));
    if let Ok(Some(_)) = child.try_wait() {
        anyhow::bail!("ssh tunnel exited during setup; see {}", dir.join(format!("{}.log", client.name)).display());
    }
    let pid = child.id();
    registry::write_private(&pid_path(&dir, &client.name), format!("{pid}\n"))?;
    registry::write_private(&port_path(&dir, &client.name), format!("{port}\n"))?;

    // Wire the queue env so tasks see the proxy without any task edits.
    let proxy = format!("socks5h://127.0.0.1:{port}");
    match env_write(t, client, "ALL_PROXY", Some(&proxy)) {
        Ok(()) => eprintln!("tunnel: {} socks5 -> 127.0.0.1:{port} (ALL_PROXY written to env)", client.name),
        Err(e) => {
            eprintln!("tunnel: up, but env write failed ({e:#}); set ALL_PROXY={proxy} manually")
        }
    }
    Ok(pid)
}

/// What `heal` did for one client whose box answered a probe.
pub enum Heal {
    /// No tunnel was ever set up — nothing to do.
    NotRecorded,
    /// Tunnel ssh is still alive.
    Healthy,
    /// Dead tunnel re-established; carries the new pid.
    Reconnected(u32),
    /// Reconnect failed; ALL_PROXY was stripped from the box's env so
    /// tasks fail over to direct egress instead of a dead proxy. State
    /// files stay — the next sweep retries the reconnect.
    Stripped,
}

/// Self-heal a dead recorded tunnel while the box itself is
/// reachable (design.md §4.4): try `up` once; on failure strip
/// ALL_PROXY so queued tasks are not poisoned by a dead proxy.
/// Called by the dispatcher sweep only on a successful probe — a
/// dead box never reaches this (env cannot be edited anyway).
pub fn heal(t: &dyn Transport, client: &ClientEntry) -> Result<Heal> {
    let dir = tunnels_dir()?;
    heal_in(&dir, t, client, |t, c| up(t, c, None))
}

/// The decision logic, with the state dir and the reconnect action
/// injected — tests exercise it without `$SHUNT_HOME` or real ssh.
fn heal_in(dir: &Path, t: &dyn Transport, client: &ClientEntry, reconnect: impl FnOnce(&dyn Transport, &ClientEntry) -> Result<u32>) -> Result<Heal> {
    if !recorded_in(dir, &client.name) {
        return Ok(Heal::NotRecorded);
    }
    if status_in(dir, &client.name).alive {
        return Ok(Heal::Healthy);
    }
    match reconnect(t, client) {
        Ok(pid) => Ok(Heal::Reconnected(pid)),
        Err(e) => {
            eprintln!("tunnel: {} dead; reconnect failed ({e:#}); stripping ALL_PROXY from env", client.name);
            // The box answered a probe this sweep, so the strip is
            // expected to work — surface a failure rather than drop it.
            env_write(t, client, "ALL_PROXY", None)?;
            Ok(Heal::Stripped)
        }
    }
}

/// Tear it down: TERM the ssh process group, drop state files, and
/// remove the ALL_PROXY line from the box's env.
pub fn down(t: &dyn Transport, client: &ClientEntry) -> Result<()> {
    let dir = tunnels_dir()?;
    if let Some(pid) = read_pid(&dir, &client.name) {
        if pid_alive(pid) {
            let _ = Command::new("kill").args([&pid.to_string()]).status();
        }
    }
    let _ = std::fs::remove_file(pid_path(&dir, &client.name));
    let _ = std::fs::remove_file(port_path(&dir, &client.name));
    if !client.local {
        // Best-effort: a dead box may not answer; the tunnel is already down.
        let _ = env_write(t, client, "ALL_PROXY", None);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::registry::SshEndpoint;
    use crate::server::transport::LocalTransport;
    use std::collections::BTreeMap;

    fn entry(root: &Path) -> ClientEntry {
        ClientEntry { name: "tun-test".into(), local: true, disabled: false, local_bin: None, timeout_s: None, ssh_timeout_s: None, shutdown_cmd: None, expire: String::new(), env_setup: String::new(), shell: String::new(), endpoint_rotates: false, ssh: SshEndpoint::default(), qroot: root.display().to_string(), deploy_root: "/".into(), facts: BTreeMap::new(), billing: None }
    }

    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("shunt-tun-{tag}-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        registry::private_dir(&dir).unwrap();
        dir
    }

    #[test]
    fn env_write_upserts_and_removes() {
        let root = tmp("env");
        let client = entry(&root);
        let t = LocalTransport { bin: PathBuf::from("/bin/sh"), timeout_s: Some(10) };
        let f = root.join("env");
        env_write(&t, &client, "ALL_PROXY", Some("socks5h://127.0.0.1:1080")).unwrap();
        env_write(&t, &client, "WORKDIR", Some("/work")).unwrap();
        let text = std::fs::read_to_string(&f).unwrap();
        assert!(text.contains("ALL_PROXY=socks5h://127.0.0.1:1080"));
        assert!(text.contains("WORKDIR=/work"));
        // Upsert replaces in place, does not duplicate.
        env_write(&t, &client, "ALL_PROXY", Some("socks5h://127.0.0.1:2080")).unwrap();
        let text = std::fs::read_to_string(&f).unwrap();
        assert_eq!(text.matches("ALL_PROXY=").count(), 1);
        assert!(text.contains(":2080"));
        // Remove drops only the named key.
        env_write(&t, &client, "ALL_PROXY", None).unwrap();
        let text = std::fs::read_to_string(&f).unwrap();
        assert!(!text.contains("ALL_PROXY"));
        assert!(text.contains("WORKDIR=/work"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn heal_not_recorded_is_noop() {
        let state = tmp("heal-nr");
        let root = tmp("heal-nr-q");
        let client = entry(&root);
        let t = LocalTransport { bin: PathBuf::from("/bin/sh"), timeout_s: Some(10) };
        let called = std::cell::Cell::new(false);
        let res = heal_in(&state, &t, &client, |_, _| {
            called.set(true);
            Ok(1)
        })
        .unwrap();
        assert!(matches!(res, Heal::NotRecorded));
        assert!(!called.get());
        std::fs::remove_dir_all(&state).ok();
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn heal_dead_tunnel_reconnects() {
        let state = tmp("heal-up");
        let root = tmp("heal-up-q");
        let client = entry(&root);
        let t = LocalTransport { bin: PathBuf::from("/bin/sh"), timeout_s: Some(10) };
        // Recorded + dead: port file only, no live pid.
        registry::write_private(&port_path(&state, "tun-test"), "1080\n").unwrap();
        let res = heal_in(&state, &t, &client, |_, _| Ok(4242)).unwrap();
        assert!(matches!(res, Heal::Reconnected(4242)));
        std::fs::remove_dir_all(&state).ok();
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn heal_dead_tunnel_strips_proxy_on_failure() {
        let state = tmp("heal-strip");
        let root = tmp("heal-strip-q");
        let client = entry(&root);
        let t = LocalTransport { bin: PathBuf::from("/bin/sh"), timeout_s: Some(10) };
        // Poisoned env + dead tunnel record (pid file naming a dead pid).
        env_write(&t, &client, "ALL_PROXY", Some("socks5h://127.0.0.1:1080")).unwrap();
        env_write(&t, &client, "WORKDIR", Some("/work")).unwrap();
        registry::write_private(&pid_path(&state, "tun-test"), "99999999\n").unwrap();
        registry::write_private(&port_path(&state, "tun-test"), "1080\n").unwrap();
        let res = heal_in(&state, &t, &client, |_, _| anyhow::bail!("ssh refused")).unwrap();
        assert!(matches!(res, Heal::Stripped));
        let text = std::fs::read_to_string(root.join("env")).unwrap();
        assert!(!text.contains("ALL_PROXY"), "{text}");
        assert!(text.contains("WORKDIR=/work"), "{text}");
        // State files stay so the next sweep retries the reconnect,
        // and status reports the intermediate state as stale.
        assert!(recorded_in(&state, "tun-test"));
        assert!(status_in(&state, "tun-test").stale);
        std::fs::remove_dir_all(&state).ok();
        std::fs::remove_dir_all(&root).ok();
    }
}
