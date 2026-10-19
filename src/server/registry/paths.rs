//! Registry home and per-name path constructors.

use crate::server::fsutil::private_dir;
use anyhow::{Context, Result};
use std::path::PathBuf;

/// Registry home: `$SHUNT_HOME` else `~/.config/shunt`.
pub fn shunt_home() -> Result<PathBuf> {
    if let Ok(h) = std::env::var("SHUNT_HOME") {
        return Ok(PathBuf::from(h));
    }
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".config/shunt"))
}

/// Dedicated keypair for a client: `$SHUNT_HOME/keys/<name>`[.pub].
/// `client add`/`bootstrap` generate it on demand (design.md §4.1).
pub fn client_key_path(name: &str) -> Result<PathBuf> {
    Ok(shunt_home()?.join("keys").join(name))
}

/// shunt-scoped known_hosts, one file per client:
/// `$SHUNT_HOME/known_hosts/<name>`. Per-client files are what make
/// two rows pointing at the same host:port usable — different boxes
/// behind one NAT endpoint carry different host keys, and a shared
/// file would pin the first TOFU'd key against the second client
/// forever (REMOTE HOST IDENTIFICATION HAS CHANGED deadlock). TOFU
/// (accept-new) keeps box keys out of the user's own ~/.ssh and
/// needs no manual maintenance. The dir is created here so callers
/// never prep it before an ssh-keygen/-R or ssh accept-new write.
pub fn known_hosts_path(name: &str) -> Result<PathBuf> {
    let dir = shunt_home()?.join("known_hosts");
    private_dir(&dir)?;
    Ok(dir.join(name))
}
