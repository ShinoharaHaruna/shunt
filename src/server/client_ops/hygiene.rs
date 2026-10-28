//! Connection hygiene: drop stale known_hosts entries and kill the
//! live ssh mux master so endpoint/key changes actually take hold.

use crate::server::registry::{self, ClientEntry};
use anyhow::{Context, Result};
use std::fs;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

struct HostkeyStage(PathBuf);

impl HostkeyStage {
    fn new(parent: &Path) -> Result<Self> {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        loop {
            // Leading '-' cannot be a client name. Exclusive creation also
            // isolates concurrent edits and never reuses a stale work directory.
            let dir = parent.join(format!("-hostkey-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
            match fs::DirBuilder::new().mode(0o700).create(&dir) {
                Ok(()) => return Ok(Self(dir)),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(err).context("create private host-key staging directory"),
            }
        }
    }
}

impl Drop for HostkeyStage {
    fn drop(&mut self) {
        if let Err(err) = fs::remove_dir_all(&self.0) {
            eprintln!("warn: could not clean host-key staging directory {} ({err}); remove it manually", self.0.display());
        }
    }
}

/// `ssh-keygen -R <spec>` scoped to one client's own known_hosts
/// file. The spec is `host` on port 22 else `[host]:port` —
/// OpenSSH's stored form. No-op when the store or the host is absent.
pub(crate) fn drop_hostkey(client: &str, host: &str, port: u16) -> Result<()> {
    let kh = registry::known_hosts_path(client)?;
    if host.is_empty() || !kh.exists() {
        return Ok(());
    }
    let spec = if port == 22 { host.to_string() } else { format!("[{host}]:{port}") };
    // OpenSSH writes <file>.old (and temporary siblings) before returning.
    // Even that backup name can be another client's store, so it must never
    // edit the live path. A same-filesystem private stage permits atomic publish.
    let stage = HostkeyStage::new(kh.parent().context("known_hosts path has no parent")?)?;
    let work = stage.0.join("known_hosts");
    let permissions = fs::metadata(&kh)?.permissions();
    fs::copy(&kh, &work).context("stage known_hosts")?;
    fs::set_permissions(&work, fs::Permissions::from_mode(0o600))?;
    let st = std::process::Command::new("ssh-keygen").args(["-R", &spec, "-f"]).arg(&work).stdin(std::process::Stdio::null()).status().context("spawn ssh-keygen -R")?;
    if !st.success() {
        anyhow::bail!("ssh-keygen -R {spec} failed");
    }
    // OpenSSH may replace the staged inode with umask-derived permissions.
    fs::set_permissions(&work, permissions)?;
    fs::rename(&work, &kh).context("replace known_hosts after ssh-keygen -R")?;
    Ok(())
}

/// Kill the live mux master for `name`, if one is up (`ssh -O exit`).
/// A master answers for the destination it was opened to regardless
/// of the requested target — after an endpoint change or key
/// rotation it must go, or the next exec rides the stale session.
pub(crate) fn kill_mux(name: &str) {
    if let Ok(sock) = crate::server::transport::cm_dir().map(|d| d.join(format!("{name}.sock"))) {
        if sock.exists() {
            let _ = std::process::Command::new("ssh").args(["-S", &sock.display().to_string(), "-O", "exit", "x"]).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
        }
    }
    drop_askpass(name);
}

/// Drop the generated `cm/<name>.askpass`. It pins the row's
/// plaintext password, so any path that kills the master — or
/// rotates the credential — must not leave the stale secret on
/// disk; the next password-only spawn regenerates it on demand.
pub(crate) fn drop_askpass(name: &str) {
    if let Ok(f) = crate::server::transport::cm_dir().map(|d| d.join(format!("{name}.askpass"))) {
        let _ = std::fs::remove_file(f);
    }
}

/// `shunt client hostkey <name>`: drop the box's host-key entries
/// from OUR known_hosts (`ssh-keygen -R` scoped to it) and kill the
/// live mux master, so the next connect re-TOFUs a rebuilt container.
/// This is the escape hatch for `REMOTE HOST IDENTIFICATION HAS
/// CHANGED` — the user's own ~/.ssh/known_hosts is never involved.
pub fn rekey(entry: &ClientEntry) -> Result<()> {
    drop_hostkey(&entry.name, &entry.ssh.host, entry.ssh.port)?;
    // The mux master still holds a session verified against the OLD key.
    kill_mux(&entry.name);
    Ok(())
}
