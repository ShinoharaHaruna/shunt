//! Idle poweroff (design.md §3.11): arm on `shutdown_on_idle`, count
//! down `shutdown_grace_s` with per-second cancels, final re-check,
//! last-moment command resolution (`shutdown_cmd` file > `$SHUTDOWN_CMD`
//! env > `/usr/bin/shutdown`). All stubs.

use anyhow::Result;
use std::path::Path;

pub const DEFAULT_GRACE_S: u64 = 60;

/// Arm idle-shutdown with an optional grace override. Stub.
pub fn arm(_root: &Path, _grace_s: Option<u64>) -> Result<()> {
    anyhow::bail!("shutdown::arm: not yet implemented")
}

/// Disarm (remove the flag). Stub.
pub fn disarm(_root: &Path) -> Result<()> {
    anyhow::bail!("shutdown::disarm: not yet implemented")
}

/// Report armed state + grace for `shutdown status`. Stub.
pub fn status(_root: &Path) -> Result<crate::proto::wire::ShutdownState> {
    anyhow::bail!("shutdown::status: not yet implemented")
}
