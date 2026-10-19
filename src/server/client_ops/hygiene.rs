//! Connection hygiene: drop stale known_hosts entries and kill the
//! live ssh mux master so endpoint/key changes actually take hold.

use crate::server::registry::{self, ClientEntry};
use anyhow::{Context, Result};

/// `ssh-keygen -R <spec>` scoped to one client's own known_hosts
/// file. The spec is `host` on port 22 else `[host]:port` —
/// OpenSSH's stored form. No-op when the store or the host is absent.
pub(crate) fn drop_hostkey(client: &str, host: &str, port: u16) -> Result<()> {
    let kh = registry::known_hosts_path(client)?;
    if host.is_empty() || !kh.exists() {
        return Ok(());
    }
    let spec = if port == 22 { host.to_string() } else { format!("[{host}]:{port}") };
    let st = std::process::Command::new("ssh-keygen").args(["-R", &spec, "-f", &kh.display().to_string()]).stdin(std::process::Stdio::null()).status().context("spawn ssh-keygen -R")?;
    if !st.success() {
        anyhow::bail!("ssh-keygen -R {spec} failed");
    }
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
