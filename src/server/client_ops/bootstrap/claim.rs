//! `.shunt-owner` claim (design.md §4.1): two independent shunt
//! installs pointed at the same qroot silently merge into one
//! queue — seq races between two dispatchers, one daemon serving
//! both. The claiming client's ssh pubkey line is the ownership
//! token; bootstrap refuses a foreign claim before any write.

use crate::server::registry::ClientEntry;

/// `<qroot>/.shunt-owner` claim as one remote script (§4.1).
/// `$0` is the owner-file path, `$1` our pubkey line. Answers a
/// marker word: OWNED (file already holds our key), CLAIMED (we
/// wrote it), FOREIGN (another install's key — refuse). `set -C`
/// (O_EXCL) makes the create atomic against a racing claimer.
pub(super) const CLAIM_SCRIPT: &str = r#"o="$0"; if [ -f "$o" ]; then grep -qxF "$1" "$o" && echo OWNED || echo FOREIGN; exit 0; fi; mkdir -p "${o%/*}"; if ( set -C; printf '%s\n' "$1" > "$o" ) 2>/dev/null; then echo CLAIMED; else grep -qxF "$1" "$o" 2>/dev/null && echo OWNED || echo FOREIGN; fi"#;

/// `<qroot>/.shunt-owner` — holds the claiming client's ssh public
/// key line; bootstrap refuses a foreign claim (§4.1).
pub(super) fn owner_path(e: &ClientEntry) -> String {
    format!("{}/.shunt-owner", e.qroot)
}

/// The pubkey identifying this install: `{ssh.key}.pub` when it
/// exists, else `ssh-keygen -y` derivation. `None` means no identity
/// to claim with (e.g. `--key` given without a derivable pub) — the
/// caller then skips the claim check.
pub(super) fn owner_pubkey(e: &ClientEntry) -> Option<String> {
    if e.ssh.key.is_empty() {
        return None;
    }
    let pub_path = format!("{}.pub", e.ssh.key);
    if let Ok(s) = std::fs::read_to_string(&pub_path) {
        let s = s.trim();
        if !s.is_empty() {
            return Some(s.to_string());
        }
    }
    let out = std::process::Command::new("ssh-keygen").args(["-y", "-f", &e.ssh.key]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then(|| format!("{s} shunt-{}", e.name))
}
