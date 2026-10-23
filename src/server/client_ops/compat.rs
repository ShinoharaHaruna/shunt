//! Binary/libc compatibility (design.md §15): the check OBSERVES
//! rather than predicts — a staged binary's own `--version 2>&1`
//! output is classified into three deploy states. The byte-scan
//! (`required_glibc`) and the box's `ldd --version` survive only as
//! diagnostic context; they never decide.

use crate::proto::libc::{parse_ldd_version, Libc};
use crate::server::registry::ClientEntry;
use crate::server::transport::Transport;

pub use crate::proto::libc::required_glibc;

/// The three deploy states (§15): what a candidate binary does on
/// THIS box, observed.
#[derive(Debug, PartialEq, Eq)]
pub enum Compat {
    /// Runs clean — the loader is silent.
    Ok,
    /// Runs, but prints noise on every invocation (weak-version
    /// refs the box's older glibc cannot serve, and the like).
    /// Starting is safe — the loader guarantees every hard ref
    /// resolved — but the noise recurs per wire call and lands in
    /// daemon.log; an unprobed weak symbol could still abort a
    /// task mid-run. Prefer a musl build when one exists.
    Warn(String),
    /// Cannot start: a hard version miss, wrong arch, a missing
    /// interpreter, or an exec failure.
    Error(String),
}

/// Classify merged `<bin> --version 2>&1` output. The version line
/// proves the process started; any other line is noise that recurs
/// on every future invocation — `warn`, not `ok`. No version line
/// at all (or an exec error, which the caller maps to `Error`)
/// means the binary never ran.
pub fn classify_smoke(out: &str) -> Compat {
    let mut version = false;
    let mut noise = Vec::new();
    for line in out.lines().map(str::trim).filter(|l| !l.is_empty()) {
        // `shunt <digit>` — the prefix alone would also match a
        // `shunt: error while loading…` loader line.
        if line.strip_prefix("shunt ").and_then(|s| s.chars().next()).is_some_and(|c| c.is_ascii_digit()) {
            version = true;
        } else {
            noise.push(line);
        }
    }
    match (version, noise.is_empty()) {
        (false, _) => Compat::Error(out.trim().to_string()),
        (true, true) => Compat::Ok,
        (true, false) => Compat::Warn(format!("loader noise: {}", noise.join(" | "))),
    }
}

/// Verdict for a binary that REPORTS IN — already running on the
/// box (`status`/`probe` self-scan vs the `libc` fact). The process
/// existing at all proves every hard ref resolved, so a need the
/// box cannot serve can only be a weak-ref miss → `warn`, never
/// `error`. A need against an undeterminable libc is still
/// attention-worthy: it runs, but nothing proves it runs quiet.
pub fn reported_verdict(needs: Option<(u32, u32)>, remote: Option<&Libc>) -> Compat {
    let Some((maj, min)) = needs else { return Compat::Ok };
    match remote {
        Some(Libc::Glibc(bm, bn)) if (*bm, *bn) >= (maj, min) => Compat::Ok,
        Some(Libc::Glibc(bm, bn)) => Compat::Warn(format!("binary needs GLIBC {maj}.{min}; box has glibc {bm}.{bn} — running on weak refs, loader warns every invocation")),
        Some(l @ Libc::Musl(_)) => Compat::Error(format!("binary needs GLIBC {maj}.{min}; box runs {l}")),
        None => Compat::Warn(format!("binary needs GLIBC {maj}.{min}; box libc undeterminable — running but unverified")),
    }
}

/// The box's libc via `ldd --version` over raw exec — diagnostic
/// context for warn/error reports, deliberately not the wire
/// `probe`. `2>&1` because the transport returns stdout only
/// (musl's ldd writes its version to stderr). None = undeterminable.
pub fn remote_libc(t: &dyn Transport, c: &ClientEntry) -> Option<Libc> {
    let out = t.exec(c, &["sh".into(), "-c".into(), "ldd --version 2>&1".into()], None).ok()?;
    parse_ldd_version(&out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::libc::scan_glibc_refs;

    #[test]
    fn scan_finds_max_glibc_ref() {
        let bin = b"\x00GLIBC_2.17\x00\xffGLIBC_2.39\nGLIBC_notnumeric";
        assert_eq!(scan_glibc_refs(bin), Some((2, 39)));
        assert_eq!(scan_glibc_refs(b"no refs here"), None);
        assert_eq!(scan_glibc_refs(b"GLIBC_"), None);
        assert_eq!(scan_glibc_refs(b"GLIBC_2."), None);
    }

    #[test]
    fn smoke_clean_version_is_ok() {
        assert_eq!(classify_smoke("shunt 0.4.0 (deadbee 2026-01-01)\n"), Compat::Ok);
    }

    #[test]
    fn smoke_loader_noise_is_warn() {
        let out = "/q/bin/shunt.new: weak version `GLIBC_2.39' not found (required by /q/bin/shunt.new)\nshunt 0.4.0 (deadbee 2026-01-01)\n";
        match classify_smoke(out) {
            Compat::Warn(w) => assert!(w.contains("GLIBC_2.39"), "{w}"),
            other => panic!("expected warn, got {other:?}"),
        }
    }

    #[test]
    fn smoke_loader_failure_is_error() {
        // The binary path printed BEFORE the colon must not be
        // mistaken for a version line.
        let out = "/q/bin/shunt.new: error while loading shared libraries: libc.so.6: version `GLIBC_2.39' not found (required by /q/bin/shunt.new)";
        assert!(matches!(classify_smoke(out), Compat::Error(_)));
        assert!(matches!(classify_smoke(""), Compat::Error(_)));
        assert!(matches!(classify_smoke("sh: exec format error"), Compat::Error(_)));
    }

    #[test]
    fn reported_verdict_matrix() {
        use Compat::*;
        // Static binary: anything goes — including a libc-less box.
        assert_eq!(reported_verdict(None, None), Ok);
        assert_eq!(reported_verdict(Some((2, 39)), Some(&Libc::Glibc(2, 39))), Ok);
        assert_eq!(reported_verdict(Some((2, 39)), Some(&Libc::Glibc(2, 40))), Ok);
        // A running binary on an older glibc can only be weak-missed.
        assert!(matches!(reported_verdict(Some((2, 39)), Some(&Libc::Glibc(2, 35))), Warn(_)));
        // Impossible in practice (a gnu binary on musl never starts),
        // but reported state must not fabricate ok.
        assert!(matches!(reported_verdict(Some((2, 17)), Some(&Libc::Musl(Some("1.2.4".into())))), Error(_)));
        assert!(matches!(reported_verdict(Some((2, 17)), None), Warn(_)));
    }
}
