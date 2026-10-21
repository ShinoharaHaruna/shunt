//! Release-asset rescue (design.md §15): when the auto binary can't
//! run on the box's libc, fetch the matching musl static build from
//! GitHub releases. The artifact self-certifies equivalence — its
//! embedded `--version` sha must equal the local binary's.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const MUSL_TARGET: &str = "x86_64-unknown-linux-musl";

/// Temp dir guard: the extracted binary is borrowed by bootstrap for
/// `copy_to`; the dir is removed when this drops.
pub struct TempBin {
    path: PathBuf,
    dir: PathBuf,
}
impl TempBin {
    pub fn path(&self) -> &Path {
        &self.path
    }
}
impl Drop for TempBin {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// `x.y.z` + optional embedded sha + dirty flag from a `--version`
/// string like `shunt 0.2.0 (faae5d9-dirty 2026-10-22)`.
fn parse_version(out: &str) -> Option<(String, Option<String>, bool)> {
    let s = out.trim().strip_prefix("shunt")?.trim();
    let (ver, rest) = s.split_once(' ').map(|(v, r)| (v, Some(r))).unwrap_or((s, None));
    let (sha, dirty) = match rest.and_then(|r| r.strip_prefix('(').and_then(|r| r.split(')').next())) {
        Some(meta) => {
            let sha = meta.split_whitespace().next().unwrap_or("");
            if sha.is_empty() {
                (None, false)
            } else if let Some(clean) = sha.strip_suffix("-dirty") {
                (Some(clean.to_string()), true)
            } else {
                (Some(sha.to_string()), false)
            }
        }
        None => (None, false),
    };
    Some((ver.to_string(), sha, dirty))
}

/// `$SHUNT_RELEASE_REPO` > `server.toml release_repo` > upstream
/// default. Empty env disables the fetch deliberately (a rescue
/// never silently disappears — the caller reports the cause).
fn resolve_repo() -> Result<String> {
    match std::env::var("SHUNT_RELEASE_REPO") {
        Ok(v) => {
            let v = v.trim();
            anyhow::ensure!(!v.is_empty(), "release fetch disabled: SHUNT_RELEASE_REPO is empty");
            Ok(v.to_string())
        }
        Err(_) => Ok(crate::server::registry::load_server_config()?.release_repo.unwrap_or_else(|| crate::UPSTREAM_REPO.into())),
    }
}

/// `<bin> --version` locally — the version/sha identity of the binary
/// we'd ship. Must succeed; an unrunnable local path is its own bug.
fn local_version(bin: &Path) -> Result<String> {
    let out = Command::new(bin).arg("--version").output().with_context(|| format!("cannot run {} --version", bin.display()))?;
    anyhow::ensure!(out.status.success(), "{} --version rc={}", bin.display(), out.status.code().unwrap_or(-1));
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Download + checksum-verify + extract + sha-verify the musl asset
/// matching the local binary's version. Err falls back to the
/// caller's hard fail — rescue is opportunistic, never silent.
pub fn rescue_musl(local: &Path) -> Result<TempBin> {
    anyhow::ensure!(std::env::consts::ARCH == "x86_64", "release assets are x86_64-only; local arch is {}", std::env::consts::ARCH);
    let (ver, sha, dirty) = parse_version(&local_version(local)?).context("cannot parse local --version output")?;
    let repo = resolve_repo()?;
    let asset = format!("shunt-v{ver}-{MUSL_TARGET}.tar.gz");
    let sums = format!("SHA256SUMS-{MUSL_TARGET}");

    let dir = std::env::temp_dir().join(format!("shunt-release-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir); // a previous run's leftovers
    std::fs::create_dir_all(&dir)?;
    // Guard: an early `?` return deletes the download dir; success
    // disarms it (the caller's TempBin owns the lifetime instead).
    let guard = TempBin { path: PathBuf::new(), dir: dir.clone() };

    download(&repo, &ver, &asset, &dir)?;
    download(&repo, &ver, &sums, &dir)?;

    // Checksum gate: a truncated or substituted tarball must not ship.
    let sum = Command::new("sha256sum").args(["--check", "--strict", &sums]).current_dir(&dir).output().context("run sha256sum")?;
    anyhow::ensure!(sum.status.success(), "checksum mismatch for {asset}: {}", String::from_utf8_lossy(&sum.stdout).trim());

    let tar = Command::new("tar").arg("-xzf").arg(dir.join(&asset)).arg("-C").arg(&dir).output().context("run tar")?;
    anyhow::ensure!(tar.status.success(), "tar failed: {}", String::from_utf8_lossy(&tar.stderr).trim());
    let shipped = dir.join(format!("shunt-v{ver}-{MUSL_TARGET}")).join("shunt");
    anyhow::ensure!(shipped.is_file(), "archive lacks the shunt binary");

    // Same-code proof: the artifact's embedded sha must equal ours —
    // a dev/dirty local build must not be swapped for a release it
    // doesn't match.
    let (aver, asha, _) = parse_version(&local_version(&shipped)?).context("release binary --version unparseable")?;
    match (&sha, &asha) {
        (Some(l), Some(r)) => anyhow::ensure!(l == r, "release build is {r}, local build is {l} — refusing to ship different code"),
        (None, _) => anyhow::ensure!(aver == ver, "release version {aver} != local {ver}"),
        (Some(_), None) => anyhow::bail!("release binary carries no sha; cannot prove same code"),
    }
    if dirty {
        eprintln!("note: local binary is dirty — the release build ships the committed code without your uncommitted changes");
    }
    std::mem::forget(guard); // keep the dir alive for the caller
    Ok(TempBin { path: shipped, dir })
}

/// gh first (own auth), then `curl -L` — public release assets need
/// no credentials.
fn download(repo: &str, ver: &str, file: &str, dir: &Path) -> Result<()> {
    let out = Command::new("gh").args(["release", "download", &format!("v{ver}"), "--repo", repo, "-p", file, "-D"]).arg(dir).arg("--clobber").stdout(Stdio::null()).stderr(Stdio::piped()).output();
    if let Ok(o) = out {
        if o.status.success() && dir.join(file).is_file() {
            return Ok(());
        }
    }
    let url = format!("https://github.com/{repo}/releases/download/v{ver}/{file}");
    let out = Command::new("curl").args(["-fsSL", "--max-time", "120", "-o"]).arg(dir.join(file)).arg(&url).output().context("curl not runnable")?;
    anyhow::ensure!(out.status.success(), "download failed for {file}: {}", String::from_utf8_lossy(&out.stderr).trim());
    Ok(())
}
