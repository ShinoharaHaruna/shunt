//! Tunnel bring-up / teardown: detached `ssh -N -R <port>`, state
//! publication, and ALL_PROXY env reconciliation.

use crate::server::registry::ClientEntry;
use crate::server::transport::{ssh_conn_args, ssh_tool_cmd, Transport};
use anyhow::{Context, Result};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use super::env::env_write;
use super::liveness::{client_target, pid_alive};
use super::state::{client_lock, pid_path, port_path, read_pid, recorded_port, remove_state_file, tunnels_dir};
use super::DEFAULT_SOCKS_PORT;

/// Open the tunnel: detached `ssh -N -R <port>`, then write
/// `ALL_PROXY=socks5h://127.0.0.1:<port>` into the box's queue env.
/// Returns the pid. Idempotent: an already-live tunnel is reported.
pub fn up(t: &dyn Transport, client: &ClientEntry, port: Option<u16>) -> Result<u32> {
    let dir = tunnels_dir()?;
    let _lock = client_lock(&dir, &client.name)?;
    up_locked(&dir, t, client, port)
}

// Callers hold the same client lock through state publication AND env edits.
// heal calls this helper rather than reacquiring its own non-reentrant flock.
pub(super) fn up_locked(dir: &Path, t: &dyn Transport, client: &ClientEntry, port: Option<u16>) -> Result<u32> {
    if client.local {
        anyhow::bail!("{} uses the loopback transport — no tunnel needed", client.name);
    }
    if let Some(pid) = read_pid(dir, &client.name) {
        if pid_alive(pid, recorded_port(dir, &client.name), client_target(client).as_deref()) {
            let port = recorded_port(dir, &client.name);
            let proxy = port.map(|p| format!("socks5h://127.0.0.1:{p}"));
            env_write(t, client, "ALL_PROXY", proxy.as_deref())?;
            return Ok(pid);
        }
    }
    let port = port.unwrap_or(DEFAULT_SOCKS_PORT);
    let log_path = dir.join(format!("{}.log", client.name));
    let log = std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(&log_path)?;
    // mode(0o600) only applies at creation — re-tighten a pre-existing
    // loose file.
    let _ = std::fs::set_permissions(&log_path, std::fs::Permissions::from_mode(0o600));

    let args = up_argv(client, port)?;
    // The tunnel's ssh has no retry loop of its own (design.md §4.1):
    // a spawn that dies during the setup window gets ONE retry behind
    // the client's pre_connect hook, and a call that starts on a
    // nonzero conn-fail streak runs the hook before the first spawn
    // rather than after another wasted ConnectTimeout — a spawn on
    // an unreachable endpoint stays "alive" past the window anyway,
    // so without the eager case the hook would never get its chance.
    let mut hook_done = client.pre_connect.is_empty();
    let mut hook_note: Option<String> = None;
    if !hook_done && crate::server::registry::conn_fail_read(&client.name) > 0 {
        hook_note = crate::server::transport::run_pre_connect(client);
        hook_done = true;
    }
    let mut child = loop {
        // ExitOnForwardFailure makes a bad bind exit fast; give it a beat.
        let mut c = ssh_tool_cmd(client, "ssh")?.args(&args[1..]).stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log.try_clone()?).process_group(0).spawn().context("spawn ssh tunnel")?;
        std::thread::sleep(Duration::from_millis(500));
        if c.try_wait().context("check ssh tunnel during setup")?.is_none() {
            break c;
        }
        if hook_done {
            let note = hook_note.as_deref().map(|n| format!(" (pre_connect {n})")).unwrap_or_default();
            anyhow::bail!("ssh tunnel exited during setup{note}; see {}", log_path.display());
        }
        hook_done = true;
        hook_note = crate::server::transport::run_pre_connect(client);
    };
    let pid = child.id();
    let mut publishing = false;
    let setup = (|| -> Result<()> {
        // Publish the PID last. A failed setup may leave a port-only record
        // for heal, but must never leave a newly spawned untracked process.
        publishing = true;
        crate::server::fsutil::write_private(&port_path(dir, &client.name), format!("{port}\n"))?;
        crate::server::fsutil::write_private(&pid_path(dir, &client.name), format!("{pid}\n"))?;
        Ok(())
    })();
    if let Err(e) = setup {
        // Never signal an already-reaped setup failure: its PID may be reused.
        // An unreaped child pins its PID while we kill the freshly spawned group.
        if !matches!(child.try_wait(), Ok(Some(_))) {
            let _ = crate::client::exec::signal_pgid(pid, "KILL");
            child.kill().ok();
        }
        child.wait().context("reap failed ssh tunnel setup")?;
        // Keep a pre-existing PID-only marker if setup never published state,
        // or port creation failed: heal must retain its retry responsibility.
        if publishing && port_path(dir, &client.name).exists() {
            remove_state_file(&pid_path(dir, &client.name))?;
        }
        return Err(e);
    }
    // Only committed tunnels may outlive their caller; the reaper prevents
    // zombies when up is used by a long-running dispatcher.
    std::thread::spawn(move || {
        let _ = child.wait();
    });

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
pub(super) fn up_argv(client: &ClientEntry, port: u16) -> Result<Vec<String>> {
    let mut args = vec!["ssh".to_string(), "-o".into(), "ControlMaster=no".into(), "-o".into(), "ControlPath=none".into()];
    args.extend(ssh_conn_args(client)?);
    args.extend(["-N".into(), "-R".into(), port.to_string(), "-o".into(), "ExitOnForwardFailure=yes".into(), "-o".into(), "ServerAliveInterval=30".into(), "-o".into(), "ServerAliveCountMax=3".into()]);
    Ok(args)
}

/// Tear it down: TERM the ssh process group, drop state files, and
/// remove the ALL_PROXY line from the box's env.
pub fn down(t: &dyn Transport, client: &ClientEntry) -> Result<()> {
    let dir = tunnels_dir()?;
    let _lock = client_lock(&dir, &client.name)?;
    down_locked(&dir, t, client)
}

fn down_locked(dir: &Path, t: &dyn Transport, client: &ClientEntry) -> Result<()> {
    if let Some(pid) = read_pid(dir, &client.name) {
        let port = recorded_port(dir, &client.name);
        let target = client_target(client);
        // Recheck identity before each signal: stale/recycled PIDs must never
        // let down kill a stranger's SSH session. Keep the record on failure.
        for signal in ["TERM", "KILL"] {
            if !pid_alive(pid, port, target.as_deref()) {
                break;
            }
            if let crate::client::exec::SignalOutcome::Failed(e) = crate::client::exec::signal_pgid(pid, signal) {
                return Err(e).context("stop ssh tunnel; retry tunnel down");
            }
            let deadline = Instant::now() + Duration::from_secs(2);
            while pid_alive(pid, port, target.as_deref()) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(25));
            }
        }
        anyhow::ensure!(!pid_alive(pid, port, target.as_deref()), "ssh tunnel {pid} did not stop; retry tunnel down");
    }
    remove_state_file(&pid_path(dir, &client.name))?;
    remove_state_file(&port_path(dir, &client.name))?;
    if !client.local {
        // Best-effort: a dead box may not answer; the tunnel is already down.
        if let Err(e) = env_write(t, client, "ALL_PROXY", None) {
            eprintln!("tunnel: stopped, but queue env cleanup failed ({e:#}); retry tunnel down");
        }
    }
    Ok(())
}
