//! Idle poweroff (design.md §3.11): arm on `shutdown_on_idle`, count
//! down `shutdown_grace_s` with per-second cancels, final re-check,
//! last-moment command resolution (`shutdown_cmd` file > `$SHUTDOWN_CMD`
//! env > `/usr/bin/shutdown`).

use anyhow::Result;
use std::path::Path;

use crate::agent::queue::{self, DEFAULT_GRACE_S};

/// Write `<qroot>/shutdown_cmd` — the poweroff command the daemon
/// execs at countdown end (design.md §3.11 resolution chain).
pub fn set_cmd(root: &Path, cmd: &str) -> Result<()> {
    std::fs::write(root.join("shutdown_cmd"), format!("{cmd}\n"))?;
    Ok(())
}

/// Arm idle-shutdown, optionally overriding the grace file.
pub fn arm(root: &Path, grace_s: Option<u64>) -> Result<()> {
    std::fs::write(root.join("shutdown_on_idle"), b"")?;
    if let Some(g) = grace_s {
        std::fs::write(root.join("shutdown_grace_s"), format!("{g}\n"))?;
    }
    Ok(())
}

/// Disarm: remove the flag. A countdown in progress cancels within 1 s.
pub fn disarm(root: &Path) -> Result<()> {
    let _ = std::fs::remove_file(root.join("shutdown_on_idle"));
    Ok(())
}

/// Grace seconds: file > default 60 (`''|0` → default, as the bash).
pub fn grace(root: &Path) -> u64 {
    let g = std::fs::read_to_string(root.join("shutdown_grace_s"))
        .ok()
        .and_then(|s| {
            let d: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
            d.parse::<u64>().ok()
        })
        .unwrap_or(0);
    if g == 0 {
        DEFAULT_GRACE_S
    } else {
        g
    }
}

/// Armed state + grace + the command that would fire.
pub fn status(root: &Path) -> Result<crate::proto::wire::ShutdownState> {
    Ok(crate::proto::wire::ShutdownState { armed: root.join("shutdown_on_idle").exists(), grace_s: grace(root), cmd: resolve_cmd(root) })
}

/// Resolve the poweroff command at the last moment: `shutdown_cmd`
/// file > `$SHUTDOWN_CMD` env > `/usr/bin/shutdown`. Last-moment
/// resolution is what lets a test redirect to a marker script.
pub fn resolve_cmd(root: &Path) -> String {
    if let Ok(c) = std::fs::read_to_string(root.join("shutdown_cmd")) {
        let c = c.trim();
        if !c.is_empty() {
            return c.to_string();
        }
    }
    std::env::var("SHUTDOWN_CMD").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/usr/bin/shutdown".to_string())
}

/// True when pending/ has at least one `.sh` file.
pub fn pending_nonempty(root: &Path) -> bool {
    queue::pending_list(root).map(|v| !v.is_empty()).unwrap_or(false)
}
