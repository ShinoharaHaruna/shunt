//! The box's C library, parsed from `ldd --version` (design.md §8,
//! §15). Shared between the box-side `probe` fact and the server-side
//! bootstrap compatibility check.

use std::fmt;

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
        if let Some((a, b)) = base.split_once('.') {
            if let (Ok(a), Ok(b)) = (a.parse::<u32>(), b.parse::<u32>()) {
                ver = Some(Libc::Glibc(a, b));
            }
        }
    }
    ver
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
}
