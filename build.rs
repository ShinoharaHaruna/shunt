//! Build metadata: pin the exact commit into the version string.
//! `shunt 0.2.0 (abc1234 2026-10-21)` — the same binary gets scp'd
//! to boxes and feedback reports cite the version, so `x.y.z` alone
//! can't distinguish two dev builds. `-dirty` flags uncommitted
//! state, because the sha then does NOT describe the code that ran.
//! Absent git or .git (crates.io tarball, vendored), the string is
//! just the crate version.

use std::process::Command;

fn main() {
    // Rebuild when the commit or the index changes; working-tree
    // edits without a stage don't retrigger (a `cargo install` build
    // re-runs anyway).
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");

    // Plain short sha + dirty flag — `git describe`'s tag-relative
    // form ("v0.1.0-4-gd870adb") reads as self-contradictory next to
    // the crate version ("0.2.0 (v0.1.0-4-…)").
    let sha = git(&["rev-parse", "--short", "HEAD"]).map(|s| if git(&["status", "--porcelain"]).is_some_and(|s| !s.is_empty()) { format!("{s}-dirty") } else { s });
    let date = git(&["log", "-1", "--format=%cd", "--date=short"]);
    let meta = match (sha, date) {
        (Some(d), Some(dt)) => format!(" ({d} {dt})"),
        (Some(d), None) => format!(" ({d})"),
        _ => String::new(),
    };
    println!("cargo:rustc-env=SHUNT_VERSION={}{meta}", env!("CARGO_PKG_VERSION"));
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}
