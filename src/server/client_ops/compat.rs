//! Binary/libc compatibility (design.md §15): a glibc-linked dev
//! build must not land silently on a box whose loader can't serve
//! it — the "weak version not found" warning is stderr noise `exec`
//! discards on success, so compatibility is checked *before* the
//! copy.

use crate::proto::libc::{parse_ldd_version, Libc};
use crate::server::registry::ClientEntry;
use crate::server::transport::Transport;
use anyhow::{Context, Result};
use std::path::Path;

/// Maximum `GLIBC_<maj>.<min>` version a binary references, found by
/// an in-process byte-scan — no `strings`/`readelf` dependency, and
/// a binary with no refs is libc-independent (musl static) and ships
/// unconditionally.
pub fn required_glibc(path: &Path) -> Result<Option<(u32, u32)>> {
    let bytes = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    Ok(scan_glibc_refs(&bytes))
}

pub fn scan_glibc_refs(bin: &[u8]) -> Option<(u32, u32)> {
    const NEEDLE: &[u8] = b"GLIBC_";
    let mut max: Option<(u32, u32)> = None;
    let mut i = 0;
    while i + NEEDLE.len() <= bin.len() {
        let Some(off) = bin[i..].windows(NEEDLE.len()).position(|w| w == NEEDLE) else { break };
        i += off + NEEDLE.len();
        let (mut maj, mut j) = (0u32, i);
        while j < bin.len() && bin[j].is_ascii_digit() {
            maj = maj * 10 + (bin[j] - b'0') as u32;
            j += 1;
        }
        if j == i || bin.get(j) != Some(&b'.') {
            continue;
        }
        j += 1;
        let (mut min, k0) = (0u32, j);
        while j < bin.len() && bin[j].is_ascii_digit() {
            min = min * 10 + (bin[j] - b'0') as u32;
            j += 1;
        }
        if j == k0 {
            continue;
        }
        max = Some(max.map_or((maj, min), |m| m.max((maj, min))));
    }
    max
}

/// The box's libc via `ldd --version` over raw exec — deliberately
/// not the wire `probe`: the binary this validates isn't installed
/// yet. `2>&1` because the transport returns stdout only (musl's
/// ldd writes its version to stderr). None = undeterminable.
pub fn remote_libc(t: &dyn Transport, c: &ClientEntry) -> Option<Libc> {
    let out = t.exec(c, &["sh".into(), "-c".into(), "ldd --version 2>&1".into()], None).ok()?;
    parse_ldd_version(&out)
}

pub enum Verdict {
    Compatible,
    /// Human-readable reason, e.g. "needs GLIBC 2.39; box has glibc 2.35".
    Incompatible(String),
}

/// Tuple-compare the requirement against the box's libc. Older glibc
/// is a refusal, not a warning — a missing weak-version ref runs "by
/// luck" until a touched code path crashes mid-task (issue #2).
pub fn check(needs: Option<(u32, u32)>, remote: Option<&Libc>) -> Verdict {
    let Some((maj, min)) = needs else { return Verdict::Compatible };
    match remote {
        Some(Libc::Glibc(bm, bn)) if (*bm, *bn) >= (maj, min) => Verdict::Compatible,
        Some(Libc::Glibc(bm, bn)) => Verdict::Incompatible(format!("binary needs GLIBC {maj}.{min}; box has glibc {bm}.{bn}")),
        Some(l @ Libc::Musl(_)) => Verdict::Incompatible(format!("binary needs GLIBC {maj}.{min}; box runs {l}")),
        None => Verdict::Incompatible(format!("binary needs GLIBC {maj}.{min}; box libc undeterminable (no `ldd --version`)")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_finds_max_glibc_ref() {
        let bin = b"\x00GLIBC_2.17\x00\xffGLIBC_2.39\nGLIBC_notnumeric";
        assert_eq!(scan_glibc_refs(bin), Some((2, 39)));
        assert_eq!(scan_glibc_refs(b"no refs here"), None);
        assert_eq!(scan_glibc_refs(b"GLIBC_"), None);
        assert_eq!(scan_glibc_refs(b"GLIBC_2."), None);
    }

    #[test]
    fn verdict_matrix() {
        use Verdict::*;
        // Static binary: anything goes — including a libc-less box.
        assert!(matches!(check(None, None), Compatible));
        assert!(matches!(check(Some((2, 39)), Some(&Libc::Glibc(2, 39))), Compatible));
        assert!(matches!(check(Some((2, 39)), Some(&Libc::Glibc(2, 40))), Compatible));
        assert!(matches!(check(Some((2, 39)), Some(&Libc::Glibc(2, 35))), Incompatible(_)));
        assert!(matches!(check(Some((2, 17)), Some(&Libc::Musl(Some("1.2.4".into())))), Incompatible(_)));
        assert!(matches!(check(Some((2, 17)), None), Incompatible(_)));
    }
}
