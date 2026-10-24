//! SshTransport unit tests: fake `ssh`/`scp` binaries ahead on PATH
//! observe argv, flap rc-255, hang past the deadline, and drive the
//! pre_connect hook paths. PATH/SHUNT_HOME are process-global, so
//! these serialize on ENV_LOCK (and HOME_LOCK where the registry
//! reads home).

use super::super::{local_client, Transport};
use super::SshTransport;
use crate::server::registry::ClientEntry;

/// Env mutation (PATH, timeout override) is process-global —
/// serialize the fake-ssh tests. Tests that also touch
/// SHUNT_HOME additionally hold the registry's HOME_LOCK so
/// registry-side env users can't observe their value.
use super::super::ENV_LOCK;

/// Install a fake `ssh` ahead on PATH; returns the tmp dir.
/// `script` is the whole shell body; argv reaches it as $@.
fn fake_ssh(script: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("shunt-fakessh-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("ssh");
    std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let old = std::env::var("PATH").unwrap_or_default();
    std::env::set_var("PATH", format!("{}:{old}", dir.display()));
    dir
}

fn restore_path() {
    if let Ok(p) = std::env::var("PATH") {
        // Drop the faked leading entry.
        std::env::set_var("PATH", p.split_once(':').map(|(_, rest)| rest).unwrap_or(&p));
    }
}

fn ssh_client() -> ClientEntry {
    let mut c = local_client();
    c.local = false;
    c.ssh.host = "127.0.0.1".into();
    c.ssh.user = "u".into();
    c
}

#[test]
fn ssh_exec_passes_serveralive() {
    let _g = ENV_LOCK.lock().unwrap();
    let dir = std::env::temp_dir().join(format!("shunt-fakessh-out-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let argfile = dir.join("argv");
    std::env::set_var("FAKE_ARGV", &argfile);
    fake_ssh("printf '%s\\n' \"$@\" > \"$FAKE_ARGV\"; exit 0");
    let out = SshTransport.exec(&ssh_client(), &["status".into(), "--json".into()], None).unwrap();
    assert_eq!(out, "");
    let argv = std::fs::read_to_string(&argfile).unwrap();
    assert!(argv.contains("ServerAliveInterval=15"), "{argv}");
    assert!(argv.contains("ServerAliveCountMax=2"), "{argv}");
    std::env::remove_var("FAKE_ARGV");
    restore_path();
}

#[test]
fn ssh_exec_drains_big_stdout() {
    let _g = ENV_LOCK.lock().unwrap();
    // >64 KiB on stdout deadlocks an undrained pipe: the remote
    // blocks on write, never exits, and the deadline would fire
    // on a healthy-but-verbose command (`logs -n 200`).
    fake_ssh("yes | head -c 200000");
    let out = SshTransport.exec(&ssh_client(), &["logs".into(), "-n".into(), "200".into()], None).unwrap();
    assert_eq!(out.len(), 200000);
    restore_path();
}

#[test]
fn ssh_exec_surfaces_remote_wire_error() {
    let _g = ENV_LOCK.lock().unwrap();
    // Wire ops fail as `{"ok":false,"error":"…"}` on stdout + rc 1;
    // the message must reach the caller, not die with the pipe.
    fake_ssh("printf '%s\\n' '{\"protocol\":1,\"ok\":false,\"error\":\"sequence space exhausted\"}'; exit 1");
    let err = SshTransport.exec(&ssh_client(), &["enqueue".into()], None).unwrap_err();
    assert!(err.to_string().contains("sequence space exhausted"), "{err}");
    restore_path();
}

/// A stale `{"ok":false}` line on an rc-255 exit must NOT take
/// the wire-error path: rc 255 is the ssh-layer signal and always
/// routes to retry/conn_fail (a mux replaying buffered output or
/// a wedged remote echoing an old reply must not mask a dead box).
#[test]
fn ssh_exec_255_with_wire_line_still_retries() {
    let _h = crate::server::registry::tests::HOME_LOCK.lock().unwrap();
    let _g = ENV_LOCK.lock().unwrap();
    let home = std::env::temp_dir().join(format!("shunt-ssh-home255-{}", std::process::id()));
    std::env::set_var("SHUNT_HOME", &home);
    let dir = std::env::temp_dir().join(format!("shunt-fakessh-255-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let count = dir.join("count");
    fake_ssh(&format!("printf '%s\\n' '{{\"protocol\":1,\"ok\":false,\"error\":\"stale\"}}'; n=$(cat {0} 2>/dev/null || echo 0); echo $((n+1)) > {0}; exit 255", count.display()));
    let err = SshTransport.exec(&ssh_client(), &["status".into(), "--json".into()], None).unwrap_err();
    assert!(err.to_string().contains("unreachable rc=255"), "{err}");
    assert_eq!(std::fs::read_to_string(&count).unwrap().trim(), "3", "rc 255 flaps must retry despite the wire line");
    std::env::remove_var("SHUNT_HOME");
    restore_path();
}

#[test]
fn ssh_exec_deadline_kills_hung_remote() {
    let _g = ENV_LOCK.lock().unwrap();
    std::env::set_var("SHUNT_SSH_EXEC_TIMEOUT_S", "1");
    fake_ssh("sleep 30");
    let err = SshTransport.exec(&ssh_client(), &["status".into(), "--json".into()], None).unwrap_err();
    assert!(err.to_string().contains("timed out"), "{err}");
    std::env::remove_var("SHUNT_SSH_EXEC_TIMEOUT_S");
    restore_path();
}

/// A failed shunt_home()/cm-dir setup must reach the caller as an
/// error — not degrade to /dev/null ControlPath + a chmodded /tmp
/// (the fallback once stripped /tmp's sticky bit).
#[test]
fn base_args_errs_when_home_unusable() {
    let _h = crate::server::registry::tests::HOME_LOCK.lock().unwrap();
    std::env::set_var("SHUNT_HOME", "/proc/shunt-cannot-exist");
    assert!(SshTransport::base_args(&ssh_client()).is_err());
    std::env::remove_var("SHUNT_HOME");
}

/// Install a fake `scp` ahead on PATH (same pattern as `fake_ssh`).
fn fake_scp(script: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("shunt-fakescp-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("scp");
    std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let old = std::env::var("PATH").unwrap_or_default();
    std::env::set_var("PATH", format!("{}:{old}", dir.display()));
    dir
}

#[test]
fn scp_retries_255_flaps_then_succeeds() {
    let _h = crate::server::registry::tests::HOME_LOCK.lock().unwrap();
    let _g = ENV_LOCK.lock().unwrap();
    let home = std::env::temp_dir().join(format!("shunt-scp-home-{}", std::process::id()));
    std::env::set_var("SHUNT_HOME", &home);
    let dir = std::env::temp_dir().join(format!("shunt-fakescp-out-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let count = dir.join("count");
    fake_scp(&format!("n=$(cat {0} 2>/dev/null || echo 0); n=$((n+1)); echo $n > {0}; [ $n -lt 3 ] && exit 255; exit 0", count.display()));
    let local = dir.join("bin");
    std::fs::write(&local, "x").unwrap();
    SshTransport.copy_to(&ssh_client(), &local, "/remote/shunt").unwrap();
    assert_eq!(std::fs::read_to_string(&count).unwrap().trim(), "3", "two 255 flaps retried inside the call");
    std::env::remove_var("SHUNT_HOME");
    restore_path();
}

#[test]
fn scp_passes_serveralive() {
    let _h = crate::server::registry::tests::HOME_LOCK.lock().unwrap();
    let _g = ENV_LOCK.lock().unwrap();
    let home = std::env::temp_dir().join(format!("shunt-scp-home4-{}", std::process::id()));
    std::env::set_var("SHUNT_HOME", &home);
    let dir = std::env::temp_dir().join(format!("shunt-fakescp-out4-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let argfile = dir.join("argv");
    std::env::set_var("FAKE_ARGV", &argfile);
    fake_scp("printf '%s\\n' \"$@\" > \"$FAKE_ARGV\"; exit 0");
    let local = dir.join("bin");
    std::fs::write(&local, "x").unwrap();
    SshTransport.copy_to(&ssh_client(), &local, "/remote/shunt").unwrap();
    let argv = std::fs::read_to_string(&argfile).unwrap();
    assert!(argv.contains("ServerAliveInterval=15"), "{argv}");
    assert!(argv.contains("ServerAliveCountMax=2"), "{argv}");
    std::env::remove_var("FAKE_ARGV");
    std::env::remove_var("SHUNT_HOME");
    restore_path();
}

#[test]
fn scp_uses_legacy_protocol_with_shell_quoted_paths() {
    let _h = crate::server::registry::tests::HOME_LOCK.lock().unwrap();
    let _g = ENV_LOCK.lock().unwrap();
    let home = std::env::temp_dir().join(format!("shunt-scp-home3-{}", std::process::id()));
    std::env::set_var("SHUNT_HOME", &home);
    let dir = std::env::temp_dir().join(format!("shunt-fakescp-out3-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let argfile = dir.join("argv");
    let marker = dir.join("injected");
    std::env::set_var("FAKE_ARGV", &argfile);
    fake_scp("printf '%s\\n' \"$@\" > \"$FAKE_ARGV\"; exit 0");
    let local = dir.join("bin");
    std::fs::write(&local, "x").unwrap();
    let paths = ["/queue with space/bin/shunt".to_string(), format!("/queue $(touch {})/bin/shunt", marker.display()), "/queue's/bin/shunt".to_string()];
    let mut captured = Vec::new();
    for path in &paths {
        SshTransport.copy_to(&ssh_client(), &local, path).unwrap();
        captured.push(std::fs::read_to_string(&argfile).unwrap());
    }
    std::env::remove_var("FAKE_ARGV");
    std::env::remove_var("SHUNT_HOME");
    restore_path();

    for (path, argv) in paths.iter().zip(captured) {
        // Quoting alone is wrong under SFTP, which preserves the
        // quotes literally. Pin the protocol as well as the path.
        assert!(argv.lines().any(|arg| arg == "-O"), "shell-quoted paths require explicit legacy SCP: {argv}");
        let remote = argv.lines().last().unwrap().strip_prefix("u@127.0.0.1:").unwrap();
        // Exercise remote shell parsing, not just our quote helper:
        // the sink must receive exactly one unchanged path argument.
        let out = std::process::Command::new("sh").arg("-c").arg(format!("printf '%s\\0' {remote}")).output().unwrap();
        assert!(out.status.success(), "{out:?}");
        assert_eq!(out.stdout, format!("{path}\0").as_bytes());
        assert!(!marker.exists(), "remote path executed a shell substitution");
    }
}

/// pre_connect (design.md §4.1): fires once after the first
/// rc-255, then the remaining retries ride whatever it
/// established.
#[test]
fn pre_connect_fires_once_on_first_255() {
    let _h = crate::server::registry::tests::HOME_LOCK.lock().unwrap();
    let _g = ENV_LOCK.lock().unwrap();
    let home = std::env::temp_dir().join(format!("shunt-ssh-pc1-{}", std::process::id()));
    std::env::set_var("SHUNT_HOME", &home);
    let dir = std::env::temp_dir().join(format!("shunt-pc1-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let count = dir.join("count");
    let marker = dir.join("hook");
    fake_ssh(&format!("n=$(cat {0} 2>/dev/null || echo 0); n=$((n+1)); echo $n > {0}; [ $n -lt 2 ] && exit 255; exit 0", count.display()));
    let mut c = ssh_client();
    c.pre_connect = format!("echo ran >> {}", marker.display());
    let out = SshTransport.exec(&c, &["status".into()], None).unwrap();
    assert_eq!(out, "");
    assert_eq!(std::fs::read_to_string(&count).unwrap().trim(), "2", "first 255 -> hook -> retry succeeds");
    assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), "ran", "hook fired exactly once");
    std::env::remove_var("SHUNT_HOME");
    restore_path();
}

/// The eager case: a call that starts on a nonzero conn_fail
/// streak runs the hook BEFORE the first attempt — not after
/// another wasted ConnectTimeout.
#[test]
fn pre_connect_runs_eagerly_on_nonzero_streak() {
    let _h = crate::server::registry::tests::HOME_LOCK.lock().unwrap();
    let _g = ENV_LOCK.lock().unwrap();
    let home = std::env::temp_dir().join(format!("shunt-ssh-pc2-{}", std::process::id()));
    std::env::set_var("SHUNT_HOME", &home);
    let dir = std::env::temp_dir().join(format!("shunt-pc2-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let order = dir.join("order");
    fake_ssh(&format!("echo ssh >> {}; exit 0", order.display()));
    let mut c = ssh_client();
    c.pre_connect = format!("echo hook >> {}", order.display());
    // Two bumps = streak 2; the success below resets it.
    crate::server::registry::conn_fail_bump(&c.name);
    crate::server::registry::conn_fail_bump(&c.name);
    SshTransport.exec(&c, &["status".into()], None).unwrap();
    assert_eq!(std::fs::read_to_string(&order).unwrap(), "hook\nssh\n", "hook must precede the first attempt when the streak is nonzero");
    std::env::remove_var("SHUNT_HOME");
    restore_path();
}

/// A failing hook never gates the retries, but its rc + stderr
/// tail must ride the unreachable error.
#[test]
fn pre_connect_failure_named_in_unreachable_error() {
    let _h = crate::server::registry::tests::HOME_LOCK.lock().unwrap();
    let _g = ENV_LOCK.lock().unwrap();
    let home = std::env::temp_dir().join(format!("shunt-ssh-pc3-{}", std::process::id()));
    std::env::set_var("SHUNT_HOME", &home);
    let dir = std::env::temp_dir().join(format!("shunt-pc3-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let count = dir.join("count");
    fake_ssh(&format!("n=$(cat {0} 2>/dev/null || echo 0); n=$((n+1)); echo $n > {0}; exit 255", count.display()));
    let mut c = ssh_client();
    c.pre_connect = "echo 'uplink is down' >&2; exit 1".into();
    let err = SshTransport.exec(&c, &["status".into()], None).unwrap_err();
    assert!(err.to_string().contains("pre_connect rc=1: uplink is down"), "{err}");
    assert_eq!(std::fs::read_to_string(&count).unwrap().trim(), "3", "a failed hook must not gate the remaining retries");
    std::env::remove_var("SHUNT_HOME");
    restore_path();
}

/// A hook hitting its own deadline is reported as `timed out`,
/// not silently dropped.
#[test]
fn pre_connect_timeout_named_in_unreachable_error() {
    let _h = crate::server::registry::tests::HOME_LOCK.lock().unwrap();
    let _g = ENV_LOCK.lock().unwrap();
    let home = std::env::temp_dir().join(format!("shunt-ssh-pc4-{}", std::process::id()));
    std::env::set_var("SHUNT_HOME", &home);
    std::env::set_var("SHUNT_PRE_CONNECT_TIMEOUT_S", "1");
    fake_ssh("exit 255");
    let mut c = ssh_client();
    c.pre_connect = "sleep 30".into();
    let err = SshTransport.exec(&c, &["status".into()], None).unwrap_err();
    assert!(err.to_string().contains("pre_connect timed out"), "{err}");
    std::env::remove_var("SHUNT_PRE_CONNECT_TIMEOUT_S");
    std::env::remove_var("SHUNT_HOME");
    restore_path();
}

/// A wire-level failure (rc 1 + `{"ok":false}`) is a REMOTE
/// error, not a reachability one — the hook must stay out of it.
#[test]
fn wire_error_does_not_trigger_pre_connect() {
    let _h = crate::server::registry::tests::HOME_LOCK.lock().unwrap();
    let _g = ENV_LOCK.lock().unwrap();
    let home = std::env::temp_dir().join(format!("shunt-ssh-pc5-{}", std::process::id()));
    std::env::set_var("SHUNT_HOME", &home);
    let dir = std::env::temp_dir().join(format!("shunt-pc5-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let marker = dir.join("hook");
    fake_ssh("printf '%s\\n' '{\"protocol\":1,\"ok\":false,\"error\":\"sequence space exhausted\"}'; exit 1");
    let mut c = ssh_client();
    c.pre_connect = format!("echo ran >> {}", marker.display());
    let err = SshTransport.exec(&c, &["enqueue".into()], None).unwrap_err();
    assert!(err.to_string().contains("sequence space exhausted"), "{err}");
    assert!(!marker.exists(), "wire errors are remote faults — the reachability hook must not fire");
    std::env::remove_var("SHUNT_HOME");
    restore_path();
}

/// A deadline-hit hang is not a flap: the exec bails without
/// retry, streak bump — or hook.
#[test]
fn deadline_hang_does_not_trigger_pre_connect() {
    let _h = crate::server::registry::tests::HOME_LOCK.lock().unwrap();
    let _g = ENV_LOCK.lock().unwrap();
    let home = std::env::temp_dir().join(format!("shunt-ssh-pc6-{}", std::process::id()));
    std::env::set_var("SHUNT_HOME", &home);
    std::env::set_var("SHUNT_SSH_EXEC_TIMEOUT_S", "1");
    let dir = std::env::temp_dir().join(format!("shunt-pc6-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let marker = dir.join("hook");
    fake_ssh("sleep 30");
    let mut c = ssh_client();
    c.pre_connect = format!("echo ran >> {}", marker.display());
    let err = SshTransport.exec(&c, &["status".into()], None).unwrap_err();
    assert!(err.to_string().contains("timed out"), "{err}");
    assert!(!marker.exists(), "a hang is not a reachability failure — the hook must not fire");
    std::env::remove_var("SHUNT_SSH_EXEC_TIMEOUT_S");
    std::env::remove_var("SHUNT_HOME");
    restore_path();
}

#[test]
fn scp_deadline_bails_without_retry() {
    let _h = crate::server::registry::tests::HOME_LOCK.lock().unwrap();
    let _g = ENV_LOCK.lock().unwrap();
    let home = std::env::temp_dir().join(format!("shunt-scp-home2-{}", std::process::id()));
    std::env::set_var("SHUNT_HOME", &home);
    std::env::set_var("SHUNT_SSH_EXEC_TIMEOUT_S", "1");
    let dir = std::env::temp_dir().join(format!("shunt-fakescp-out2-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let count = dir.join("count");
    fake_scp(&format!("n=$(cat {0} 2>/dev/null || echo 0); echo $((n+1)) > {0}; sleep 30", count.display()));
    let local = dir.join("bin");
    std::fs::write(&local, "x").unwrap();
    let err = SshTransport.copy_to(&ssh_client(), &local, "/remote/shunt").unwrap_err();
    assert!(err.to_string().contains("timed out"), "{err}");
    assert_eq!(std::fs::read_to_string(&count).unwrap().trim(), "1", "a hang is not a flap: no retry");
    std::env::remove_var("SHUNT_SSH_EXEC_TIMEOUT_S");
    std::env::remove_var("SHUNT_HOME");
    restore_path();
}
