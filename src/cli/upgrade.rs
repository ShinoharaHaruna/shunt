//! `shunt upgrade` (design.md §16): self-upgrade from the GitHub
//! release; `-c` upgrades a box's binary instead — musl static is
//! always the remote pick (runs on any libc), delivered through the
//! normal bootstrap path so compat/owner/smoke all still apply.

use crate::cli::args::UpgradeArgs;
use crate::server::client_ops::{self, compat, release};
use crate::server::registry;
use crate::server::transport::{self, shell_quote, shunt_bin};
use anyhow::{Context, Result};
use std::path::PathBuf;

pub fn cmd_upgrade(a: &UpgradeArgs) -> Result<()> {
    let latest = release::latest_version()?;
    match &a.client {
        Some(name) => upgrade_remote(a, name, &latest),
        None => upgrade_local(a, &latest),
    }
}

/// Local self-upgrade: pick the asset matching THIS binary's linkage
/// (glibc-linked → gnu asset, static → musl), verify, stage-then-mv
/// over `current_exe`.
fn upgrade_local(a: &UpgradeArgs, latest: &str) -> Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    let newer = release::semver_newer(latest, current);
    if a.check || a.json {
        let j = serde_json::json!({"action": "check", "current": current, "latest": latest, "newer": newer});
        if a.json {
            println!("{j}");
            return Ok(());
        }
    }
    if a.check {
        println!("{}", if newer { format!("new release available: {current} → {latest}") } else { format!("up to date ({current})") });
        return Ok(());
    }
    if !newer && !a.force {
        println!("up to date ({current})");
        return Ok(());
    }

    let exe = std::env::current_exe().context("current_exe")?;
    let target = if compat::required_glibc(&exe)?.is_some() { release::GNU_TARGET } else { release::MUSL_TARGET };
    let tb = release::fetch_asset(latest, target)?;
    verify_release(tb.path(), latest)?;
    install_over(tb.path(), &exe)?;
    if a.json {
        println!("{}", serde_json::json!({"action": "upgraded", "from": current, "to": latest, "target": target, "path": exe.display().to_string()}));
    } else {
        println!("upgraded {current} → {latest} ({target}) at {}", exe.display());
    }
    Ok(())
}

/// Box upgrade: the musl static asset (runs on every libc — the §15
/// rescue's pick is simply the default here) through `bootstrap`,
/// which keeps the owner-claim, stage-then-mv and smoke checks.
/// Skips when the box already reports the target version.
fn upgrade_remote(a: &UpgradeArgs, name: &str, latest: &str) -> Result<()> {
    let c = registry::load_client(name)?;
    let t = transport::transport_for(&c);
    let bin = shunt_bin(&c);
    let cur = t.exec(&c, &["sh".into(), "-c".into(), format!("{} --version 2>&1", shell_quote(&bin))], None).context("remote --version")?;
    let curver = release::parse_version(&cur).map(|(v, _, _)| v).unwrap_or_else(|| "?".into());
    let newer = release::semver_newer(latest, &curver);
    if a.check || a.json {
        let j = serde_json::json!({"action": "check", "client": name, "current": curver, "latest": latest, "newer": newer});
        if a.json {
            println!("{j}");
            return Ok(());
        }
    }
    if a.check {
        println!("{name}: {}", if newer { format!("new release available: {curver} → {latest}") } else { format!("up to date ({curver})") });
        return Ok(());
    }
    if !newer && !a.force {
        println!("{name}: up to date ({curver})");
        return Ok(());
    }
    anyhow::ensure!(t.exec(&c, &["uname".into(), "-m".into()], None).map(|s| s.trim() == "x86_64").unwrap_or(false), "release assets are x86_64-only; {name} is not x86_64 — upgrade it by shipping a locally built binary: `client bootstrap {name} --bin-path <build>`");
    let tb = release::fetch_asset(latest, release::MUSL_TARGET)?;
    verify_release(tb.path(), latest)?;
    client_ops::bootstrap(t.as_ref(), name, tb.path().to_str())
}

/// The downloaded binary must report the tag's version — a stale or
/// swapped asset never installs.
fn verify_release(bin: &std::path::Path, ver: &str) -> Result<()> {
    let (v, sha, _) = release::parse_version(&release::local_version(bin)?).context("release binary --version unparseable")?;
    anyhow::ensure!(v == ver, "release asset reports {v}, expected {ver} — refusing to install");
    if let Some(s) = sha {
        eprintln!("release build: {s}");
    }
    Ok(())
}

/// Stage-then-rename over the running exe (ETXTBSY-safe, atomic
/// rename on the same filesystem). A read-only install dir (system
/// package, sudo-installed) gets remediation instead of a raw error.
fn install_over(src: &std::path::Path, dst: &PathBuf) -> Result<()> {
    let staged = dst.with_file_name(format!("{}.new", dst.file_name().map(|f| f.to_string_lossy()).unwrap_or("shunt".into())));
    std::fs::copy(src, &staged).with_context(|| format!("cannot stage upgrade at {}", staged.display()))?;
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755));
    std::fs::rename(&staged, dst).with_context(|| format!("cannot replace {} — reinstall via your package manager or fix the install dir's permissions", dst.display()))?;
    Ok(())
}
