//! The box's C library, parsed from `ldd --version` (design.md §8,
//! §15). Shared between the box-side `probe`/`status` self-report and
//! the server-side bootstrap compatibility machinery — including the
//! `GLIBC_x.y` byte-scan that decides whether a binary is libc-
//! independent (musl static) or carries glibc version references.

use anyhow::{Context, Result};
use std::fmt;
use std::path::Path;

/// `Glibc(major, minor)`; `Musl` carries its version when reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Libc {
    Glibc(u32, u32),
    Musl(Option<String>),
}

impl fmt::Display for Libc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Libc::Glibc(a, b) => write!(f, "glibc {a}.{b}"),
            Libc::Musl(Some(v)) => write!(f, "musl {v}"),
            Libc::Musl(None) => write!(f, "musl"),
        }
    }
}

/// `ldd --version` output variants:
///   glibc — `ldd (Ubuntu GLIBC 2.35-0ubuntu3.8) 2.35`,
///           `ldd (GNU libc) 2.39`
///   musl  — `musl libc (x86_64)` / `Version 1.2.4` / ...
/// Anything unparseable → None (undeterminable — callers decide
/// whether that's a fail, see §15).
pub fn parse_ldd_version(out: &str) -> Option<Libc> {
    let lower = out.to_lowercase();
    if lower.contains("musl") {
        // "Version 1.2.4" — the token right after `Version`.
        let mut it = out.split_whitespace();
        while let Some(t) = it.next() {
            if t == "Version" {
                return Some(Libc::Musl(it.next().map(|v| v.trim_matches(|c: char| !(c.is_ascii_alphanumeric() || c == '.')).to_string())));
            }
        }
        return Some(Libc::Musl(None));
    }
    // glibc: the last `x.y`-shaped token is the version (the parenthesized
    // distro string may carry an earlier `2.35-0ubuntu3.8` candidate).
    let mut ver = None;
    for t in out.split_whitespace() {
        let t = t.trim_matches(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '-'));
        let base = t.split('-').next().unwrap_or(t);
        if let Some((a, b)) = parse_ver(base) {
            ver = Some(Libc::Glibc(a, b));
        }
    }
    ver
}

/// The Display form back: `"glibc 2.35"` → `Glibc(2,35)`,
/// `"musl 1.2.4"`/`"musl"` → `Musl`. Registry facts store the
/// rendered string; this is the read side.
pub fn parse_libc(s: &str) -> Option<Libc> {
    let s = s.trim();
    if let Some(v) = s.strip_prefix("glibc ") {
        let (a, b) = parse_ver(v)?;
        return Some(Libc::Glibc(a, b));
    }
    s.strip_prefix("musl").map(|v| match v.trim() {
        "" => Libc::Musl(None),
        v => Libc::Musl(Some(v.to_string())),
    })
}

/// `"2.39"` → `(2,39)` — the wire `needs_glibc` shape and the
/// version-token parser shared by the ldd scanner.
pub fn parse_ver(s: &str) -> Option<(u32, u32)> {
    let (a, b) = s.split_once('.')?;
    Some((a.parse().ok()?, b.parse().ok()?))
}

/// Maximum `GLIBC_<maj>.<min>` version a binary references, found by
/// an in-process byte-scan — no `strings`/`readelf` dependency, and
/// a binary with no refs is libc-independent (musl static) and ships
/// unconditionally.
pub fn required_glibc(path: &Path) -> Result<Option<(u32, u32)>> {
    let bytes = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    Ok(scan_glibc_refs(&bytes))
}

/// This binary's own glibc need — what the box-side `status`/`probe`
/// self-report sends (design.md §15). `current_exe` is the installed
/// `<qroot>/bin/shunt` on the wire path, i.e. exactly the binary
/// every remote call execs.
pub fn self_glibc_need() -> Option<(u32, u32)> {
    required_glibc(&std::env::current_exe().ok()?).ok().flatten()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_glibc_variants() {
        assert_eq!(parse_ldd_version("ldd (Ubuntu GLIBC 2.35-0ubuntu3.8) 2.35"), Some(Libc::Glibc(2, 35)));
        assert_eq!(parse_ldd_version("ldd (GNU libc) 2.39"), Some(Libc::Glibc(2, 39)));
    }

    #[test]
    fn parses_musl() {
        let out = "musl libc (x86_64)\nVersion 1.2.4\nUsage: ldd ...";
        assert_eq!(parse_ldd_version(out), Some(Libc::Musl(Some("1.2.4".into()))));
        assert_eq!(parse_ldd_version("musl libc (aarch64)"), Some(Libc::Musl(None)));
    }

    #[test]
    fn garbage_is_unknown() {
        assert_eq!(parse_ldd_version(""), None);
        assert_eq!(parse_ldd_version("ldd: unrecognized option"), None);
    }

    #[test]
    fn display_roundtrips() {
        assert_eq!(parse_libc("glibc 2.35"), Some(Libc::Glibc(2, 35)));
        assert_eq!(parse_libc("musl 1.2.4"), Some(Libc::Musl(Some("1.2.4".into()))));
        assert_eq!(parse_libc("musl"), Some(Libc::Musl(None)));
        assert_eq!(parse_libc("glibc"), None);
        assert_eq!(parse_libc(""), None);
        for l in [Libc::Glibc(2, 35), Libc::Musl(Some("1.2.4".into())), Libc::Musl(None)] {
            assert_eq!(parse_libc(&l.to_string()), Some(l));
        }
    }

    #[test]
    fn parses_ver() {
        assert_eq!(parse_ver("2.39"), Some((2, 39)));
        assert_eq!(parse_ver("2"), None);
        assert_eq!(parse_ver("a.b"), None);
    }

    #[test]
    fn scan_finds_max_glibc_ref() {
        let bin = b"\x00GLIBC_2.17\x00\xffGLIBC_2.39\nGLIBC_notnumeric";
        assert_eq!(scan_glibc_refs(bin), Some((2, 39)));
        assert_eq!(scan_glibc_refs(b"no refs here"), None);
        assert_eq!(scan_glibc_refs(b"GLIBC_"), None);
        assert_eq!(scan_glibc_refs(b"GLIBC_2."), None);
    }
}
