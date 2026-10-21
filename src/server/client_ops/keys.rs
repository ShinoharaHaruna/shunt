//! Dedicated per-client keypair: generate-or-reuse (`ensure_key`)
//! and password-auth install of the `.pub` (`deploy_key`).

use crate::server::registry::{self, ClientEntry};
use anyhow::{Context, Result};

/// Generate (or reuse) the dedicated keypair at
/// `$SHUNT_HOME/keys/<name>` and point `ssh.key` at it. Called by
/// `client add` and `bootstrap` — ssh becomes registration-transparent.
pub fn ensure_key(entry: &mut ClientEntry) -> Result<()> {
    if entry.local {
        return Ok(()); // loopback needs no key
    }
    if !entry.ssh.key.is_empty() && std::path::Path::new(&entry.ssh.key).exists() {
        return Ok(()); // user-supplied or previously generated key
    }
    if !entry.ssh.key.is_empty() {
        // A set-but-missing path is a user error; surface it instead
        // of silently swapping in a fresh key.
        anyhow::bail!("ssh.key points at {} but the file is missing", entry.ssh.key);
    }
    let path = registry::client_key_path(&entry.name)?;
    if let Some(p) = path.parent() {
        crate::server::fsutil::private_dir(p)?;
    }
    if !path.exists() {
        let st = std::process::Command::new("ssh-keygen").args(["-t", "ed25519", "-N", "", "-f", &path.display().to_string(), "-C", &format!("shunt-{}", entry.name)]).stdin(std::process::Stdio::null()).status().context("spawn ssh-keygen")?;
        if !st.success() {
            anyhow::bail!("ssh-keygen failed for {}", path.display());
        }
    }
    entry.ssh.key = path.display().to_string();
    Ok(())
}

/// Install `<key>.pub` on the box when `ssh.password` is set (AutoDL
/// default: password auth). One connection, password-only auth (no
/// pubkey offers — boxes with small MaxAuthTries drop the connection
/// after too many), pubkey appended via stdin with a `grep -qxF`
/// dedupe. Returns `true` when the key was deployed; `false` when no
/// password is configured and the `.pub` must be installed once via
/// the vendor console or a manual ssh-copy-id.
pub fn deploy_key(entry: &ClientEntry) -> Result<bool> {
    if entry.ssh.password.is_empty() {
        return Ok(false);
    }
    let have_sshpass = std::process::Command::new("sh").args(["-c", "command -v sshpass >/dev/null 2>&1"]).status().map(|s| s.success()).unwrap_or(false);
    if !have_sshpass {
        anyhow::bail!("ssh.password is set but `sshpass` is not installed — install it or deploy {}.pub manually", entry.ssh.key);
    }
    let pub_key = std::fs::read_to_string(format!("{}.pub", entry.ssh.key)).with_context(|| format!("cannot read {}.pub", entry.ssh.key))?.trim().to_string();
    let kh = registry::known_hosts_path(&entry.name)?.display().to_string();
    // ControlPath=none: the password-auth connection must not become
    // the mux master — later key-based execs would ride a session
    // authenticated the wrong way.
    let script = "umask 077; mkdir -p ~/.ssh; touch ~/.ssh/authorized_keys; k=$(cat); grep -qxF \"$k\" ~/.ssh/authorized_keys || echo \"$k\" >> ~/.ssh/authorized_keys";
    // `sshpass -e` reads the password from the SSHPASS env var — `-p`
    // would expose it in local /proc/*/cmdline. ServerAlive pair +
    // the shared hard deadline: §5 requires every ssh op bounded
    // twice — ConnectTimeout alone leaves a half-open TCP session to
    // a flapping box hanging `bootstrap` forever.
    use std::os::unix::process::CommandExt;
    let mut child = std::process::Command::new("sshpass").args(["-e", "ssh", "-p", &entry.ssh.port.to_string(), "-o", "PreferredAuthentications=password", "-o", "PubkeyAuthentication=no", "-o", "NumberOfPasswordPrompts=1", "-o", "StrictHostKeyChecking=accept-new", "-o", &format!("UserKnownHostsFile={kh}"), "-o", "ControlPath=none", "-o", &format!("ConnectTimeout={}", entry.ssh_timeout_s.unwrap_or(15)), "-o", "ServerAliveInterval=15", "-o", "ServerAliveCountMax=2", &format!("{}@{}", entry.ssh.user, entry.ssh.host), script]).env("SSHPASS", &entry.ssh.password).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).process_group(0).spawn().context("spawn sshpass ssh")?;
    // Write from a thread: a remote that never reads stdin while
    // filling stderr would otherwise deadlock the write — the pipe
    // readers only start inside wait_deadline. process_group(0)
    // lets the deadline kill the whole group (pgid == child pid).
    let mut w = child.stdin.take().expect("stdin piped");
    std::thread::spawn(move || {
        use std::io::Write;
        let _ = w.write_all(pub_key.as_bytes());
    });
    let out = crate::server::transport::wait_deadline(child, crate::server::transport::exec_timeout_s(), "sshpass key deploy")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        eprintln!("warning: sshpass key deploy failed (rc {:?}): {stderr} — bootstrap will likely fail; deploy {}.pub manually", out.status.code(), entry.ssh.key);
    }
    Ok(out.status.success())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::registry::{tests::HOME_LOCK, ClientEntry, SshEndpoint};
    use crate::server::transport::ENV_LOCK;
    use std::collections::BTreeMap;

    fn pw_client(key: &std::path::Path) -> ClientEntry {
        ClientEntry { name: "t".into(), local: false, disabled: false, local_bin: None, bin_path: None, timeout_s: None, ssh_timeout_s: None, shutdown_cmd: None, expire: String::new(), env_setup: String::new(), shell: String::new(), endpoint_rotates: false, ssh: SshEndpoint { host: "127.0.0.1".into(), port: 22, user: "u".into(), key: key.display().to_string(), password: "pw".into() }, qroot: "/tmp/qt".into(), deploy_root: "/tmp".into(), facts: BTreeMap::new(), billing: None }
    }

    /// Install a fake `sshpass` ahead on PATH (fake-ssh pattern).
    fn fake_sshpass(script: &str) {
        let dir = std::env::temp_dir().join(format!("shunt-fakesshpass-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("sshpass"), format!("#!/bin/sh\n{script}\n")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.join("sshpass"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let old = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{old}", dir.display()));
    }

    /// A half-open ssh session (fake sleeps past the deadline) must
    /// be killed by wait_deadline — a bare wait() hung bootstrap
    /// forever (design.md §5).
    #[test]
    fn deploy_key_deadline_kills_hung_ssh() {
        let _h = HOME_LOCK.lock().unwrap();
        let _g = ENV_LOCK.lock().unwrap();
        let home = std::env::temp_dir().join(format!("shunt-deploy-home-{}", std::process::id()));
        std::env::set_var("SHUNT_HOME", &home);
        std::env::set_var("SHUNT_SSH_EXEC_TIMEOUT_S", "1");
        let dir = std::env::temp_dir().join(format!("shunt-deploy-key-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let key = dir.join("id");
        std::fs::write(dir.join("id.pub"), "ssh-ed25519 AAAA t\n").unwrap();
        fake_sshpass("sleep 30");
        let err = deploy_key(&pw_client(&key)).unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
        std::env::remove_var("SHUNT_SSH_EXEC_TIMEOUT_S");
        std::env::remove_var("SHUNT_HOME");
    }
}
