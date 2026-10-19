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
    // would expose it in local /proc/*/cmdline.
    let mut child = std::process::Command::new("sshpass").args(["-e", "ssh", "-p", &entry.ssh.port.to_string(), "-o", "PreferredAuthentications=password", "-o", "PubkeyAuthentication=no", "-o", "NumberOfPasswordPrompts=1", "-o", "StrictHostKeyChecking=accept-new", "-o", &format!("UserKnownHostsFile={kh}"), "-o", "ControlPath=none", "-o", &format!("ConnectTimeout={}", entry.ssh_timeout_s.unwrap_or(15)), &format!("{}@{}", entry.ssh.user, entry.ssh.host), script]).env("SSHPASS", &entry.ssh.password).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().context("spawn sshpass ssh")?;
    use std::io::Write;
    child.stdin.as_mut().context("stdin piped")?.write_all(pub_key.as_bytes())?;
    drop(child.stdin.take());
    let st = child.wait().context("wait sshpass")?;
    if !st.success() {
        eprintln!("warning: sshpass key deploy failed (rc {:?}) — bootstrap will likely fail; deploy {}.pub manually", st.code(), entry.ssh.key);
    }
    Ok(st.success())
}
