//! Shared permission infrastructure (design.md §4.1): registry
//! rows, spool entries, task ledger, and tunnel state all carry
//! secrets-adjacent data, so dir/file creation is funneled through
//! these two helpers.

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
/// path keeps its own permissions): an out-of-scope `dir` is created
/// but never chmodded.
pub(crate) fn private_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir)?;
    // Chmod scope: only paths at-or-under $SHUNT_HOME are provably
    // shunt-owned. When shunt_home() fails (HOME and SHUNT_HOME both
    // unset — cron/systemd/forced-command contexts) nothing is
    // provably in-scope, so chmod nothing: the previous else-branch
    // chmodded `dir` unconditionally and, paired with a caller's
    // /tmp fallback, stripped /tmp's sticky bit to 0700 — breaking
    // every program on the box.
    let Ok(home) = crate::server::registry::shunt_home() else {
        return Ok(());
    };
    let Ok(rel) = dir.strip_prefix(&home) else {
        return Ok(());
    };
    let mut p = home;
    let _ = fs::set_permissions(&p, fs::Permissions::from_mode(0o700));
    for comp in rel.components() {
        p.push(comp.as_os_str());
        let _ = fs::set_permissions(&p, fs::Permissions::from_mode(0o700));
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// private_dir's chmod scope (design.md §4.1): dirs inside
    /// $SHUNT_HOME are tightened to 0700, dirs outside keep their
    /// own mode. Regression test for the unconditional-chmod bug:
    /// an out-of-scope dir (the /tmp fallback shape) must not lose
    /// its mode.
    #[test]
    fn private_dir_scope() {
        let _g = crate::server::registry::tests::HOME_LOCK.lock().unwrap();
        let home = std::env::temp_dir().join(format!("shunt-futil-home-{}", std::process::id()));
        std::env::set_var("SHUNT_HOME", &home);
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;

        // In-scope: created and tightened, parents included.
        private_dir(&home.join("cm")).unwrap();
        assert_eq!(mode(&home), 0o700);
        assert_eq!(mode(&home.join("cm")), 0o700);

        // Out-of-scope: created, never chmodded.
        let outside = std::env::temp_dir().join(format!("shunt-futil-outside-{}", std::process::id()));
        private_dir(&outside).unwrap();
        assert_ne!(mode(&outside), 0o700, "outside dir must keep its own permissions");
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o755)).unwrap();
        private_dir(&outside).unwrap();
        assert_eq!(mode(&outside), 0o755, "pre-existing outside dir untouched");

        std::env::remove_var("SHUNT_HOME");
        fs::remove_dir_all(&home).ok();
        fs::remove_dir_all(&outside).ok();
    }
}
