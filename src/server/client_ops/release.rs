//! Release-asset rescue (design.md §15): when the auto binary can't
//! run on the box's libc, fetch the matching musl static build from
//! GitHub releases. The artifact self-certifies equivalence — its
//! embedded `--version` sha must equal the local binary's.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub(crate) const MUSL_TARGET: &str = "x86_64-unknown-linux-musl";
pub(crate) const GNU_TARGET: &str = "x86_64-unknown-linux-gnu";

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
pub(crate) fn parse_version(out: &str) -> Option<(String, Option<String>, bool)> {
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
pub(crate) fn resolve_repo() -> Result<String> {
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
pub(crate) fn local_version(bin: &Path) -> Result<String> {
    let out = Command::new(bin).arg("--version").output().with_context(|| format!("cannot run {} --version", bin.display()))?;
    anyhow::ensure!(out.status.success(), "{} --version rc={}", bin.display(), out.status.code().unwrap_or(-1));
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Parse records without letting manifest-controlled paths reach the checksum tool.
/// Duplicate target records are ambiguous even when their digests agree.
fn expected_checksum<'a>(manifest: &'a str, asset: &str) -> Result<&'a str> {
    let mut expected = None;
    for line in manifest.lines() {
        let (digest, rest) = line.split_once(' ').context("malformed checksum entry")?;
        let file = rest.strip_prefix(' ').or_else(|| rest.strip_prefix('*')).context("malformed checksum entry mode")?;
        anyhow::ensure!(digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()), "malformed checksum digest");
        anyhow::ensure!(!file.is_empty() && file != "." && file != ".." && !file.contains(['/', '\\']), "checksum entry must name a plain filename");
        if file == asset {
            anyhow::ensure!(expected.is_none(), "ambiguous checksum entries for {asset}");
            expected = Some(digest);
        }
    }
    expected.with_context(|| format!("checksum manifest lacks {asset}"))
}

/// Release identity keeps the comparison version separate from artifact naming.
pub(crate) struct Release {
    pub version: String,
    pub tag: String,
}

impl Release {
    fn from_tag(tag: &str) -> Result<Self> {
        let tag = tag.trim();
        let version = tag.strip_prefix('v').unwrap_or(tag);
        anyhow::ensure!(!version.is_empty() && tag.bytes().all(|b| b.is_ascii_alphanumeric() || b".-+".contains(&b)), "invalid release tag: {tag}");
        Ok(Self { version: version.into(), tag: tag.into() })
    }
}

/// Download + checksum-verify + extract version `ver`'s asset for
/// `target`. Integrity is proven (SHA256SUMS-<target>); identity is
/// the CALLER's job — rescue checks same-sha, upgrade checks the
/// extracted binary reports `ver`.
#[allow(dead_code)] // Preserve the shared version-based helper's v-prefixed contract.
pub fn fetch_asset(ver: &str, target: &str) -> Result<TempBin> {
    fetch_tag_asset(&format!("v{ver}"), target)
}

/// Fetch an exact release tag; unlike the legacy version helper, no prefix is added.
pub(crate) fn fetch_tag_asset(tag: &str, target: &str) -> Result<TempBin> {
    anyhow::ensure!(std::env::consts::ARCH == "x86_64", "release assets are x86_64-only; local arch is {}", std::env::consts::ARCH);
    let repo = resolve_repo()?;
    let asset = format!("shunt-{tag}-{target}.tar.gz");
    let sums = format!("SHA256SUMS-{target}");

    let dir = std::env::temp_dir().join(format!("shunt-release-{}-{}", std::process::id(), target));
    let _ = std::fs::remove_dir_all(&dir); // a previous run's leftovers
    std::fs::create_dir_all(&dir)?;
    // Guard: an early `?` return deletes the download dir; success
    // disarms it (the caller's TempBin owns the lifetime instead).
    let guard = TempBin { path: PathBuf::new(), dir: dir.clone() };

    download(&repo, tag, &asset, &dir)?;
    download(&repo, tag, &sums, &dir)?;

    let manifest = std::fs::read_to_string(dir.join(&sums)).context("read checksum manifest")?;
    let expected = expected_checksum(&manifest, &asset)?;
    // A successful manifest-wide check can verify an unrelated file and leave
    // this archive unchecked. Hash only the archive we are about to extract.
    let sum = Command::new("sha256sum").arg("--").arg(&asset).current_dir(&dir).output().context("run sha256sum")?;
    anyhow::ensure!(sum.status.success(), "checksum failed for {asset}: {}", String::from_utf8_lossy(&sum.stderr).trim());
    let output = String::from_utf8_lossy(&sum.stdout);
    let actual = output.split_whitespace().next().context("checksum tool returned no digest")?;
    anyhow::ensure!(actual.eq_ignore_ascii_case(expected), "checksum mismatch for {asset}");

    let tar = Command::new("tar").arg("-xzf").arg(dir.join(&asset)).arg("-C").arg(&dir).output().context("run tar")?;
    anyhow::ensure!(tar.status.success(), "tar failed: {}", String::from_utf8_lossy(&tar.stderr).trim());
    let shipped = dir.join(format!("shunt-{tag}-{target}")).join("shunt");
    anyhow::ensure!(shipped.is_file(), "archive lacks the shunt binary");

    std::mem::forget(guard); // keep the dir alive for the caller
    Ok(TempBin { path: shipped, dir })
}

/// Download + checksum-verify + extract + sha-verify the musl asset
/// matching the local binary's version. Err falls back to the
/// caller's hard fail — rescue is opportunistic, never silent.
pub fn rescue_musl(local: &Path) -> Result<TempBin> {
    let (ver, sha, dirty) = parse_version(&local_version(local)?).context("cannot parse local --version output")?;
    let repo = resolve_repo()?;
    let mut found = None;
    // Only a confirmed missing tag permits trying the other supported spelling.
    // Once selected, its integrity/identity failures must never select other code.
    for tag in [format!("v{ver}"), ver.clone()] {
        if let Some(release) = query_release(&repo, Some(&tag))? {
            anyhow::ensure!(release.tag == tag && release.version == ver, "release query returned unexpected tag {} for {tag}", release.tag);
            found = Some(release);
            break;
        }
    }
    let release = found.with_context(|| format!("no release found for {ver}"))?;
    let tb = fetch_tag_asset(&release.tag, MUSL_TARGET)?;
    let shipped = tb.path();

    // Same-code proof: the artifact's embedded sha must equal ours —
    // a dev/dirty local build must not be swapped for a release it
    // doesn't match.
    let (aver, asha, _) = parse_version(&local_version(shipped)?).context("release binary --version unparseable")?;
    match (&sha, &asha) {
        (Some(l), Some(r)) => anyhow::ensure!(l == r, "release build is {r}, local build is {l} — refusing to ship different code"),
        (None, _) => anyhow::ensure!(aver == ver, "release version {aver} != local {ver}"),
        (Some(_), None) => anyhow::bail!("release binary carries no sha; cannot prove same code"),
    }
    if dirty {
        eprintln!("note: local binary is dirty — the release build ships the committed code without your uncommitted changes");
    }
    Ok(tb)
}

/// Newest non-prerelease release version (legacy normalized-version helper).
#[allow(dead_code)] // Preserve normalized output for existing version-only callers.
pub fn latest_version() -> Result<String> {
    Ok(latest_release()?.version)
}

pub(crate) fn latest_release() -> Result<Release> {
    let repo = resolve_repo()?;
    query_release(&repo, None)?.context("latest release not found")
}

/// gh errors are not reliably typed; only an API 404 proves a tag is absent.
fn query_release(repo: &str, tag: Option<&str>) -> Result<Option<Release>> {
    let mut cmd = Command::new("gh");
    cmd.args(["release", "view"]);
    if let Some(tag) = tag {
        cmd.arg(tag);
    }
    let out = cmd.args(["--repo", repo, "--json", "tagName", "-q", ".tagName"]).output();
    if let Ok(o) = out {
        if o.status.success() {
            let tag = String::from_utf8_lossy(&o.stdout);
            if !tag.trim().is_empty() {
                return Release::from_tag(&tag).map(Some);
            }
        }
    }
    let endpoint = tag.map(|t| format!("tags/{t}")).unwrap_or_else(|| "latest".into());
    let url = format!("https://api.github.com/repos/{repo}/releases/{endpoint}");
    let mut cmd = Command::new("curl");
    if tag.is_some() {
        cmd.args(["-sSL", "--write-out", "\n%{http_code}"]);
    } else {
        cmd.arg("-fsSL");
    }
    let out = cmd.args(["--max-time", "30", &url]).output().context("curl not runnable")?;
    anyhow::ensure!(out.status.success(), "release query failed for {repo}: {}", String::from_utf8_lossy(&out.stderr).trim());
    let body = String::from_utf8(out.stdout).context("release answer is not UTF-8")?;
    let body = if tag.is_some() {
        let (body, status) = body.rsplit_once('\n').context("release query lacks HTTP status")?;
        if status == "404" {
            return Ok(None);
        }
        anyhow::ensure!(status == "200", "release query failed for {repo}: HTTP {status}");
        body
    } else {
        &body
    };
    let j: serde_json::Value = serde_json::from_str(body).context("release answer is not JSON")?;
    let tag = j["tag_name"].as_str().context("release carries no tag_name")?;
    Release::from_tag(tag).map(Some)
}

/// `a` strictly newer than `b`? Numeric triples; a bare release
/// outranks its own prereleases. Unparseable input is never newer.
pub fn semver_newer(a: &str, b: &str) -> bool {
    fn parts(s: &str) -> Option<(u64, u64, u64, bool)> {
        let s = s.trim().trim_start_matches('v');
        let (core, is_rel) = match s.split_once('-') {
            Some((c, _)) => (c, false),
            None => (s, true),
        };
        let mut it = core.split('.');
        let n = |i: &mut std::str::Split<'_, char>| i.next()?.parse().ok();
        Some((n(&mut it)?, n(&mut it)?, n(&mut it)?, is_rel))
    }
    match (parts(a), parts(b)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

/// gh first (own auth), then `curl -L` — public release assets need
/// no credentials.
fn download(repo: &str, tag: &str, file: &str, dir: &Path) -> Result<()> {
    let out = Command::new("gh").args(["release", "download", tag, "--repo", repo, "-p", file, "-D"]).arg(dir).arg("--clobber").stdout(Stdio::null()).stderr(Stdio::piped()).output();
    if let Ok(o) = out {
        if o.status.success() && dir.join(file).is_file() {
            return Ok(());
        }
    }
    let url = format!("https://github.com/{repo}/releases/download/{tag}/{file}");
    let out = Command::new("curl").args(["-fsSL", "--max-time", "120", "-o"]).arg(dir.join(file)).arg(&url).output().context("curl not runnable")?;
    anyhow::ensure!(out.status.success(), "download failed for {file}: {}", String::from_utf8_lossy(&out.stderr).trim());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::semver_newer;

    #[test]
    fn semver_newer_orders() {
        assert!(semver_newer("0.3.1", "0.3.0"));
        assert!(semver_newer("0.4.0", "0.3.9"));
        assert!(semver_newer("1.0.0", "0.99.9"));
        assert!(semver_newer("0.3.0", "0.3.0-rc.1"), "release outranks its prerelease");
        assert!(semver_newer("v0.3.1", "0.3.0"), "v-prefix tolerated");
        assert!(!semver_newer("0.3.0", "0.3.0"));
        assert!(!semver_newer("0.2.9", "0.3.0"));
        assert!(!semver_newer("garbage", "0.3.0"));
        assert!(!semver_newer("0.3.0", "garbage"), "unparseable current is never newer");
    }
}

#[cfg(test)]
#[path = "release/tests.rs"]
mod tag_tests;
