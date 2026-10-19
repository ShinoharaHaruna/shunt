//! Registry permission infrastructure: shared with spool, tasks,
//! and tunnel, which is why these are `pub(crate)` rather than
//! private to the registry.

use anyhow::Result;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// Registry privacy (design.md §4.1: ssh.password is plaintext —
/// "keep registry private"). `create_dir_all`/`fs::write` follow the
/// process umask, which can leave rows group/world-readable, so every
/// shunt-managed dir under $SHUNT_HOME is forced to 0700 and every
/// file to 0600 — including re-tightening paths that predate the fix.
/// Nothing outside $SHUNT_HOME is touched (a user-supplied ssh.key
/// path keeps its own permissions).
pub(crate) fn private_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir)?;
    if let Ok(home) = super::paths::shunt_home() {
        if let Ok(rel) = dir.strip_prefix(&home) {
            let mut p = home;
            let _ = fs::set_permissions(&p, fs::Permissions::from_mode(0o700));
            for comp in rel.components() {
                p.push(comp.as_os_str());
                let _ = fs::set_permissions(&p, fs::Permissions::from_mode(0o700));
            }
            return Ok(());
        }
    }
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// Write a file 0600 regardless of umask (mode on create + explicit
/// chmod to re-tighten a pre-existing loose file).
pub(crate) fn write_private(path: &Path, contents: impl AsRef<[u8]>) -> Result<()> {
    {
        let mut f = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
        f.write_all(contents.as_ref())?;
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}
