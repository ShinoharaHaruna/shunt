//! Build metadata: pin the exact commit into the version string.
//! `shunt 0.2.0 (abc1234 2026-10-21)` — the same binary gets scp'd
//! to boxes and feedback reports cite the version, so `x.y.z` alone
//! can't distinguish two dev builds. `-dirty` flags uncommitted
//! state, because the sha then does NOT describe the code that ran.
//! Absent git or .git (crates.io tarball, vendored), the string is
//! just the crate version.

use std::process::Command;

fn main() {
    embed_web_assets();
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
    // Component vars let `version --json` report fields directly
    // instead of parsing the composed display string.
    println!("cargo:rustc-env=SHUNT_GIT_SHA={}", sha.clone().unwrap_or_default());
    println!("cargo:rustc-env=SHUNT_GIT_DATE={}", date.clone().unwrap_or_default());
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

/// Bake `web/dist` into the binary as a static (path, bytes) table.
/// `shunt serve` ships inside the same scp'd binary as everything
/// else, so the UI cannot be an external file dependency. A fresh
/// clone (or crates.io tarball) has no dist — embed a placeholder
/// page naming the build recipe instead of failing the Rust build:
/// the web UI is optional cargo-side even though it is the point
/// of `serve`.
fn embed_web_assets() {
    use std::fmt::Write as _;
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let dist = std::path::Path::new("web/dist");
    let mut files = Vec::new();
    if dist.is_dir() {
        let mut stack = vec![dist.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.is_file() {
                    files.push(p);
                }
            }
        }
        files.sort();
    }
    // rerun-if-changed on each file AND the dir itself: a new file
    // bumps the dir's mtime; an edited file bumps its own.
    println!("cargo:rerun-if-changed={}", dist.display());
    for f in &files {
        println!("cargo:rerun-if-changed={}", f.display());
    }
    let mut rs = String::from("pub static WEB_ASSETS: &[(&str, &[u8])] = &[\n");
    if files.is_empty() {
        let _ = writeln!(rs, "    (\"index.html\", PLACEHOLDER_INDEX.as_bytes()),");
    } else {
        for f in &files {
            let rel = f.strip_prefix(dist).unwrap().to_string_lossy().replace('\\', "/");
            let _ = writeln!(rs, "    ({rel:?}, include_bytes!(\"{}\") as &[u8]),", f.canonicalize().unwrap().display());
        }
    }
    rs.push_str("];\n\n");
    if files.is_empty() {
        rs.push_str(
            r###"pub const PLACEHOLDER_INDEX: &str = r##"<!doctype html>
<html><head><meta charset="utf-8"><title>shunt serve</title>
<style>body{font-family:-apple-system,system-ui,sans-serif;background:#0b0b0d;color:#e6e6eb;display:grid;place-items:center;min-height:100vh;margin:0}div{max-width:34em}code{background:#1c1c21;padding:.15em .4em;border-radius:6px}</style>
</head><body><div><h1>web UI not built</h1>
<p>This binary was compiled without <code>web/dist</code>. Build the frontend, then rebuild:</p>
<pre><code>pnpm -C web install
pnpm -C web build   # or: just web</code></pre>
<p>API endpoints (<code>/api/snapshot</code>, <code>/ws</code>) work regardless.</p>
</div></body></html>"##;
"###,
        );
    }
    std::fs::write(std::path::Path::new(&out_dir).join("web_assets.rs"), rs).unwrap();
}
