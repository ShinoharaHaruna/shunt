//! `client env`: push the row's env_setup/shell snippets to files
//! under `<qroot>` on the box.

use crate::server::registry::{self, ClientEntry};
use crate::server::transport::Transport;
use anyhow::Result;

/// `shunt client env <name>`: push the row's `env_setup`/`shell` to
/// `<qroot>/env_setup` + `<qroot>/shell` on the box. Empty fields
/// remove the files so the runner falls back to bash/none. The
/// row's residency preference is pushed the same way:
/// `exit_on_idle = true` plants the flag file that tells the daemon
/// to exit once the queue drains (§3.11a); false removes it.
pub fn push_env(t: &dyn Transport, name: &str) -> Result<()> {
    let e = registry::load_client(name)?;
    write_qroot_file(t, &e, "env_setup", &e.env_setup)?;
    write_qroot_file(t, &e, "shell", &e.shell)?;
    write_qroot_file(t, &e, "exit_on_idle", if e.exit_on_idle { "1\n" } else { "" })?;
    Ok(())
}

/// Write-or-remove one small file under `<qroot>` over the client's
/// transport. Empty content removes the file.
fn write_qroot_file(t: &dyn Transport, e: &ClientEntry, fname: &str, content: &str) -> Result<()> {
    let path = format!("{}/{}", e.qroot, fname);
    if content.is_empty() {
        t.exec(e, &["rm".into(), "-f".into(), path], None)?;
        return Ok(());
    }
    t.exec(e, &["mkdir".into(), "-p".into(), e.qroot.clone()], None)?;
    t.exec(e, &["sh".into(), "-c".into(), "cat > \"$0\"".into(), path], Some(content))?;
    Ok(())
}
