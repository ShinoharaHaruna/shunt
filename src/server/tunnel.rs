//! Box-side SOCKS5 proxy via SSH reverse dynamic forward
//! (design.md §4.3): `ssh -N -R <port>` opens `127.0.0.1:<port>` as a
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
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
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
t=$(mktemp "$1/.env.XXXXXX") || exit 1
trap 'rm -f "$t"' EXIT
grep -v "^$k=" "$f" > "$t" 2>/dev/null || true
[ -n "$v" ] && echo "$k=$v" >> "$t"
mv "$t" "$f"
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

/// `user@host` needle for cmdline identity — only when the row
/// carries a real endpoint (local rows have neither field).
fn client_target(client: &ClientEntry) -> Option<String> {
    if client.local || client.ssh.user.is_empty() || client.ssh.host.is_empty() {
        None
    } else {
        Some(format!("{}@{}", client.ssh.user, client.ssh.host))
    }
}

/// NUL-separated `/proc/<pid>/cmdline` → our tunnel when argv[0]'s
/// basename is `ssh` AND the recorded needles are present: `-R`
/// followed by the recorded port (when a port file exists) and the
/// `user@host` target (when the caller knows the client row). A bare
/// `ssh -R` match accepted ANY reverse forward — a stranger's tunnel
/// recycling our recorded pid would then pass liveness and `down`
/// would TERM it. Whole-element matching, same discipline as the
/// daemon's `is_daemon_cmdline` (design.md §3.4).
fn is_tunnel_cmdline(cmdline: &[u8], port: Option<u16>, target: Option<&str>) -> bool {
    let argv: Vec<&[u8]> = cmdline.split(|&b| b == 0).filter(|a| !a.is_empty()).collect();
    let Some(argv0) = argv.first() else { return false };
    let base = argv0.rsplit(|&b| b == b'/').next().unwrap_or(argv0);
    if base != b"ssh" {
        return false;
    }
    let r_ok = match port {
        // `-R <port>` as an adjacent pair — the exact form `up` spawns.
        Some(p) => {
            let ps = p.to_string();
            argv.windows(2).any(|w| w[0] == b"-R" && w[1] == ps.as_bytes())
        }
        None => argv.iter().any(|a| *a == b"-R"),
    };
    if !r_ok {
        return false;
    }
    match target {
        Some(t) => argv.contains(&t.as_bytes()),
        None => true,
    }
}

fn pid_alive(pid: u32, port: Option<u16>, target: Option<&str>) -> bool {
    // Cmdline identity, not bare /proc existence: a recycled pid must
    // not pass for a live tunnel (status lies, `up` early-bails, and
    // `down` would signal a foreign process).
    std::fs::read(format!("/proc/{pid}/cmdline")).ok().is_some_and(|c| is_tunnel_cmdline(&c, port, target))
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

fn recorded_port(dir: &Path, name: &str) -> Option<u16> {
    std::fs::read_to_string(port_path(dir, name)).ok().and_then(|s| s.trim().parse::<u16>().ok())
}

/// `target` is the `user@host` needle when the caller knows the
/// client row — pass `None` when the row is unloadable (liveness
/// then falls back to the port needle alone).
fn status_in(dir: &Path, name: &str, target: Option<&str>) -> TunnelState {
    let port = recorded_port(dir, name);
    let alive = matches!(read_pid(dir, name), Some(pid) if pid_alive(pid, port, target));
    let stale = !alive && recorded_in(dir, name);
    TunnelState { alive, port, stale }
}

/// Current tunnel state for a client (no network needed).
pub fn status(name: &str) -> Result<TunnelState> {
    // The row gives the `user@host` needle; an unloadable/missing
    // row degrades to port-only matching rather than failing status.
    let target = registry::load_client(name).ok().and_then(|c| client_target(&c));
    Ok(status_in(&tunnels_dir()?, name, target.as_deref()))
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
        if pid_alive(pid, recorded_port(&dir, &client.name), client_target(client).as_deref()) {
            return Ok(pid);
        }
    }
    let port = port.unwrap_or(DEFAULT_SOCKS_PORT);
    crate::server::fsutil::private_dir(&dir)?;
    let log_path = dir.join(format!("{}.log", client.name));
    let log = std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(&log_path)?;
    // mode(0o600) only applies at creation — re-tighten a pre-existing
    // loose file.
    let _ = std::fs::set_permissions(&log_path, std::fs::Permissions::from_mode(0o600));
    let err = log.try_clone()?;

    let args = up_argv(client, port)?;
    let mut child = Command::new("ssh").args(&args[1..]).stdin(Stdio::null()).stdout(log).stderr(err).process_group(0).spawn().context("spawn ssh tunnel")?;
    // ExitOnForwardFailure makes a bad bind exit fast; give it a beat.
    std::thread::sleep(std::time::Duration::from_millis(500));
    if let Ok(Some(_)) = child.try_wait() {
        anyhow::bail!("ssh tunnel exited during setup; see {}", dir.join(format!("{}.log", client.name)).display());
    }
    let pid = child.id();
    // The tunnel must outlive `up`'s caller (the dispatcher calls this
    // from heal), so the ssh stays detached — but a dropped Child is
    // still our child and zombies on death until wait()ed. Hand it to
    // a reaper thread: non-blocking for us, reaped for the kernel.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    crate::server::fsutil::write_private(&pid_path(&dir, &client.name), format!("{pid}\n"))?;
    crate::server::fsutil::write_private(&port_path(&dir, &client.name), format!("{port}\n"))?;

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
/// reachable (design.md §4.3): try `up` once; on failure strip
/// ALL_PROXY so queued tasks are not poisoned by a dead proxy.
/// Called by the dispatcher sweep only on a successful probe — a
/// dead box never reaches this (env cannot be edited anyway).
pub fn heal(t: &dyn Transport, client: &ClientEntry) -> Result<Heal> {
    let dir = tunnels_dir()?;
    // Reconnect on the recorded port, not the default: a tunnel
    // brought up with `--port 2080` must self-heal onto 2080 — the
    // port file and the box's ALL_PROXY both name it.
    let port = std::fs::read_to_string(port_path(&dir, &client.name)).ok().and_then(|s| s.trim().parse::<u16>().ok());
    let alive = status_in(&dir, &client.name, client_target(client).as_deref()).alive;
    heal_in(&dir, t, client, alive, |t, c| up(t, c, port))
}

/// The decision logic, with the state dir, liveness verdict and the
/// reconnect action injected — tests exercise it without
/// `$SHUNT_HOME`, real ssh, or a fake `/proc` cmdline.
fn heal_in(dir: &Path, t: &dyn Transport, client: &ClientEntry, alive: bool, reconnect: impl FnOnce(&dyn Transport, &ClientEntry) -> Result<u32>) -> Result<Heal> {
    if !recorded_in(dir, &client.name) {
        return Ok(Heal::NotRecorded);
    }
    if alive {
        // Liveness alone is not health (design.md §4.3): `up` warns
        // and continues when its env write fails, leaving a live
        // tunnel whose box env lacks ALL_PROXY — tasks then bypass
        // the proxy while status claims they route through it.
        // env_write is an idempotent grep-upsert, so re-assert the
        // line for the recorded port every sweep; with no recorded
        // port the right proxy line is unknowable — strip it so
        // tasks fail over to direct egress instead of a phantom
        // proxy (same rationale as the dead path).
        match recorded_port(dir, &client.name) {
            Some(p) => env_write(t, client, "ALL_PROXY", Some(&format!("socks5h://127.0.0.1:{p}")))?,
            None => env_write(t, client, "ALL_PROXY", None)?,
        }
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

/// The tunnel's ssh argv: the shared conn flags (auth, known_hosts,
/// timeouts, target) PLUS the long-lived session options. The mux
/// options are overridden OFF: `ssh_conn_args` carries
/// `ControlMaster=auto`/`ControlPath=<cm>/<name>.sock`, which would
/// either make the tunnel itself the mux master (pinning the socket
/// and routing every later exec through this session) or join an
/// existing master (so `ssh -O exit` / master death kills the
/// forward invisibly to the pid bookkeeping). ssh resolves each `-o`
/// parameter first-value-wins, so the `none` overrides must precede
/// the inherited args.
fn up_argv(client: &ClientEntry, port: u16) -> Result<Vec<String>> {
    let mut args = vec!["ssh".to_string(), "-o".into(), "ControlMaster=no".into(), "-o".into(), "ControlPath=none".into()];
    args.extend(ssh_conn_args(client)?);
    args.extend(["-N".into(), "-R".into(), port.to_string(), "-o".into(), "ExitOnForwardFailure=yes".into(), "-o".into(), "ServerAliveInterval=30".into(), "-o".into(), "ServerAliveCountMax=3".into()]);
    Ok(args)
}

/// Tear it down: TERM the ssh process group, drop state files, and
/// remove the ALL_PROXY line from the box's env.
pub fn down(t: &dyn Transport, client: &ClientEntry) -> Result<()> {
    let dir = tunnels_dir()?;
    if let Some(pid) = read_pid(&dir, &client.name) {
        // pid_alive already cmdline-verified the pid as our ssh
        // (port + user@host needles), so this can never signal a
        // recycled pid or a stranger's -R forward. The tunnel spawned
        // with process_group(0) — pid == pgid — so TERM the whole
        // group and any children go with the leader.
        if pid_alive(pid, recorded_port(&dir, &client.name), client_target(client).as_deref()) {
            let _ = crate::agent::exec::signal_pgid(pid, "TERM");
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
        crate::server::fsutil::private_dir(&dir).unwrap();
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
        let res = heal_in(&state, &t, &client, false, |_, _| {
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
        crate::server::fsutil::write_private(&port_path(&state, "tun-test"), "1080\n").unwrap();
        let res = heal_in(&state, &t, &client, false, |_, _| Ok(4242)).unwrap();
        assert!(matches!(res, Heal::Reconnected(4242)));
        std::fs::remove_dir_all(&state).ok();
        std::fs::remove_dir_all(&root).ok();
    }

    /// Live ssh but env missing ALL_PROXY (up's env_write failed):
    /// heal must re-assert the line for the recorded port, not
    /// report Healthy over a silent proxy bypass.
    #[test]
    fn heal_live_tunnel_repairs_env() {
        let state = tmp("heal-env");
        let root = tmp("heal-env-q");
        let client = entry(&root);
        let t = LocalTransport { bin: PathBuf::from("/bin/sh"), timeout_s: Some(10) };
        // Live tunnel recorded on 2080; env carries only a foreign
        // key — ALL_PROXY was never written (or was lost to an env
        // file rewrite on the box).
        crate::server::fsutil::write_private(&pid_path(&state, "tun-test"), "1234\n").unwrap();
        crate::server::fsutil::write_private(&port_path(&state, "tun-test"), "2080\n").unwrap();
        env_write(&t, &client, "WORKDIR", Some("/work")).unwrap();
        let res = heal_in(&state, &t, &client, true, |_, _| anyhow::bail!("must not reconnect a live tunnel")).unwrap();
        assert!(matches!(res, Heal::Healthy));
        let text = std::fs::read_to_string(root.join("env")).unwrap();
        assert!(text.contains("ALL_PROXY=socks5h://127.0.0.1:2080"), "{text}");
        assert!(text.contains("WORKDIR=/work"), "{text}");
        std::fs::remove_dir_all(&state).ok();
        std::fs::remove_dir_all(&root).ok();
    }

    /// Live tunnel but the port file is gone: the correct ALL_PROXY
    /// line is unknowable, so heal strips rather than leave a line
    /// that may point at a dead port.
    #[test]
    fn heal_live_tunnel_no_port_strips_env() {
        let state = tmp("heal-nop");
        let root = tmp("heal-nop-q");
        let client = entry(&root);
        let t = LocalTransport { bin: PathBuf::from("/bin/sh"), timeout_s: Some(10) };
        crate::server::fsutil::write_private(&pid_path(&state, "tun-test"), "1234\n").unwrap();
        env_write(&t, &client, "ALL_PROXY", Some("socks5h://127.0.0.1:1080")).unwrap();
        env_write(&t, &client, "WORKDIR", Some("/work")).unwrap();
        let res = heal_in(&state, &t, &client, true, |_, _| anyhow::bail!("must not reconnect a live tunnel")).unwrap();
        assert!(matches!(res, Heal::Healthy));
        let text = std::fs::read_to_string(root.join("env")).unwrap();
        assert!(!text.contains("ALL_PROXY"), "{text}");
        assert!(text.contains("WORKDIR=/work"), "{text}");
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
        crate::server::fsutil::write_private(&pid_path(&state, "tun-test"), "99999999\n").unwrap();
        crate::server::fsutil::write_private(&port_path(&state, "tun-test"), "1080\n").unwrap();
        let res = heal_in(&state, &t, &client, false, |_, _| anyhow::bail!("ssh refused")).unwrap();
        assert!(matches!(res, Heal::Stripped));
        let text = std::fs::read_to_string(root.join("env")).unwrap();
        assert!(!text.contains("ALL_PROXY"), "{text}");
        assert!(text.contains("WORKDIR=/work"), "{text}");
        // State files stay so the next sweep retries the reconnect,
        // and status reports the intermediate state as stale.
        assert!(recorded_in(&state, "tun-test"));
        assert!(status_in(&state, "tun-test", None).stale);
        std::fs::remove_dir_all(&state).ok();
        std::fs::remove_dir_all(&root).ok();
    }

    /// The tunnel ssh must ride NO mux: `ControlPath=none` (and
    /// `ControlMaster=no`) must precede the inherited
    /// `ControlPath=<cm>/<name>.sock` from `ssh_conn_args` — ssh
    /// resolves each `-o` first-value-wins. Auth/known_hosts options
    /// (-p, -i, UserKnownHostsFile) must survive.
    #[test]
    fn tunnel_argv_disables_mux_before_conn_args() {
        let _h = crate::server::registry::tests::HOME_LOCK.lock().unwrap();
        let _g = crate::server::transport::ENV_LOCK.lock().unwrap();
        let home = std::env::temp_dir().join(format!("shunt-tun-home-{}", std::process::id()));
        std::env::set_var("SHUNT_HOME", &home);
        let mut c = entry(Path::new("/q"));
        c.local = false;
        c.ssh.user = "u".into();
        c.ssh.host = "h".into();
        c.ssh.port = 2222;
        c.ssh.key = "/k".into();
        let argv = up_argv(&c, 1080).unwrap();
        std::env::remove_var("SHUNT_HOME");
        // First occurrence of each -o parameter is the effective one.
        let pos = |needle: &str| argv.iter().position(|a| a == needle).unwrap_or(usize::MAX);
        assert!(pos("ControlPath=none") < argv.iter().position(|a| a.starts_with("ControlPath=") && *a != "ControlPath=none").unwrap_or(usize::MAX), "{argv:?}");
        assert!(pos("ControlMaster=no") < pos("ControlMaster=auto"), "{argv:?}");
        assert!(argv.contains(&"/k".to_string()) && argv.contains(&"u@h".to_string()), "{argv:?}");
    }

    /// A bare `ssh -R` cmdline must NOT pass liveness when the
    /// recorded port and user@host needles are absent — a stranger's
    /// reverse forward recycling our pid would otherwise be TERM'd
    /// by `tunnel down`.
    #[test]
    fn cmdline_needs_port_and_target_needles() {
        // Build /proc-style NUL-separated cmdlines without escape soup.
        let cl = |args: &[&str]| args.join("\0").into_bytes();
        let ours = cl(&["ssh", "-i", "key", "-N", "-R", "1080", "user@host"]);
        assert!(is_tunnel_cmdline(&ours, Some(1080), Some("user@host")));
        // Foreign -R on another port: rejected once the port is known.
        let foreign = cl(&["ssh", "-N", "-R", "9999", "other@box"]);
        assert!(!is_tunnel_cmdline(&foreign, Some(1080), Some("user@host")));
        assert!(!is_tunnel_cmdline(&foreign, Some(1080), None));
        // Same port, wrong target: still not ours.
        let wrong_target = cl(&["ssh", "-N", "-R", "1080", "other@box"]);
        assert!(!is_tunnel_cmdline(&wrong_target, Some(1080), Some("user@host")));
        // No recorded port: any `-R` element satisfies the port needle.
        assert!(is_tunnel_cmdline(&foreign, None, Some("other@box")));
        // argv0 basename match: /usr/bin/ssh counts, not-ssh does not.
        assert!(is_tunnel_cmdline(&cl(&["/usr/bin/ssh", "-R", "1080"]), Some(1080), None));
        assert!(!is_tunnel_cmdline(&cl(&["autossh", "-R", "1080"]), Some(1080), None));
    }
}
