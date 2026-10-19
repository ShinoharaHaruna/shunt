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

fn pid_path(name: &str) -> Result<PathBuf> {
    Ok(tunnels_dir()?.join(format!("{name}.pid")))
}

fn port_path(name: &str) -> Result<PathBuf> {
    Ok(tunnels_dir()?.join(format!("{name}.port")))
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
}

fn read_pid(name: &str) -> Result<Option<u32>> {
    let p = pid_path(name)?;
    let Ok(s) = std::fs::read_to_string(&p) else {
        return Ok(None);
    };
    Ok(s.trim().parse().ok())
}

fn pid_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// A tunnel was set up for `name` at some point: a pid or port
/// state file exists, so the box's env may still carry ALL_PROXY
/// even after the ssh process died (endpoint drift, reboot).
pub fn recorded(name: &str) -> Result<bool> {
    Ok(pid_path(name)?.exists() || port_path(name)?.exists())
}

/// Current tunnel state for a client (no network needed).
pub fn status(name: &str) -> Result<TunnelState> {
    let port = port_path(name)?.exists().then(|| std::fs::read_to_string(port_path(name).unwrap())).and_then(|r| r.ok()).and_then(|s| s.trim().parse::<u16>().ok());
    Ok(match read_pid(name)? {
        Some(pid) if pid_alive(pid) => TunnelState { alive: true, port },
        _ => TunnelState { alive: false, port },
    })
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
    if let Some(pid) = read_pid(&client.name)? {
        if pid_alive(pid) {
            return Ok(pid);
        }
    }
    let port = port.unwrap_or(DEFAULT_SOCKS_PORT);
    let dir = tunnels_dir()?;
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
    registry::write_private(&pid_path(&client.name)?, format!("{pid}\n"))?;
    registry::write_private(&port_path(&client.name)?, format!("{port}\n"))?;

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

/// Tear it down: TERM the ssh process group, drop state files, and
/// remove the ALL_PROXY line from the box's env.
pub fn down(t: &dyn Transport, client: &ClientEntry) -> Result<()> {
    if let Some(pid) = read_pid(&client.name)? {
        if pid_alive(pid) {
            let _ = Command::new("kill").args([&pid.to_string()]).status();
        }
    }
    let _ = std::fs::remove_file(pid_path(&client.name)?);
    let _ = std::fs::remove_file(port_path(&client.name)?);
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

    #[test]
    fn env_write_upserts_and_removes() {
        let root = std::env::temp_dir().join(format!("shunt-tun-{}", std::process::id()));
        registry::private_dir(&root).unwrap();
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
}
