//! `shunt upgrade` (design.md §16): self-upgrade from the GitHub
//! release; `-c` upgrades a box's binary instead — musl static is
//! always the remote pick (runs on any libc), delivered through the
//! normal bootstrap path so compat/owner/smoke all still apply.

use crate::cli::args::UpgradeArgs;
use crate::server::client_ops::{self, compat, release};
use crate::server::registry;
use crate::server::remote;
use crate::server::transport::{self, shell_quote, shunt_bin};
use anyhow::{Context, Result};
use std::path::PathBuf;

pub fn cmd_upgrade(a: &UpgradeArgs) -> Result<()> {
    anyhow::ensure!(a.queue.is_none() || a.client.is_some(), "--queue requires -c CLIENT for a remote upgrade");
    // Same -c resolution as the rest of the server CLI: case-fold
    // substitutes canonical spelling with a note (§4.1).
    let client = a.client.as_deref().map(crate::cli::server::canonical_client).transpose()?;
    if let Some(name) = &client {
        let c = registry::load_client(name)?;
        anyhow::ensure!(a.queue.is_some() || registry::queue_names(&c).len() == 1, "{name} has multiple queue binaries; select each explicitly with `shunt upgrade -c {name} --queue NAME` (list with `shunt queue ls -c {name}`)");
        registry::resolve_queue(&c, a.queue.as_deref())?;
    }
    let latest = release::latest_release()?;
    match &client {
        Some(name) => upgrade_remote(a, name, &latest),
        None => upgrade_local(a, &latest),
    }
}

/// Local self-upgrade: pick the asset matching THIS binary's linkage
/// (glibc-linked → gnu asset, static → musl), verify, stage-then-mv
/// over `current_exe`.
fn upgrade_local(a: &UpgradeArgs, release: &release::Release) -> Result<()> {
    let latest = release.version.as_str();
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
    let tb = release::fetch_tag_asset(&release.tag, target)?;
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
/// Skips when the box already reports the target version. `name` is
/// the already-resolved canonical client name.
fn upgrade_remote(a: &UpgradeArgs, name: &str, release: &release::Release) -> Result<()> {
    let latest = release.version.as_str();
    let resolved = registry::load_queue(name, a.queue.as_deref())?;
    let queue = &resolved.name;
    let c = &resolved.client;
    let t = transport::transport_for(c);
    let bin = shunt_bin(c);
    let cur = t.exec(c, &["sh".into(), "-c".into(), format!("{} --version 2>&1", shell_quote(&bin))], None).context("remote --version")?;
    let curver = release::parse_version(&cur).map(|(v, _, _)| v).unwrap_or_else(|| "?".into());
    let newer = release::semver_newer(latest, &curver);
    if a.check || a.json {
        let j = serde_json::json!({"action": "check", "client": name, "queue": queue, "current": curver, "latest": latest, "newer": newer});
        if a.json {
            println!("{j}");
            return Ok(());
        }
    }
    if a.check {
        println!("{name}/{queue}: {}", if newer { format!("new release available: {curver} → {latest}") } else { format!("up to date ({curver})") });
        return Ok(());
    }
    if !newer && !a.force {
        println!("{name}/{queue}: up to date ({curver})");
        return Ok(());
    }
    anyhow::ensure!(t.exec(c, &["uname".into(), "-m".into()], None).map(|s| s.trim() == "x86_64").unwrap_or(false), "release assets are x86_64-only; {name} is not x86_64 — upgrade it by shipping a locally built binary: `client bootstrap {name} --bin-path <build>`");
    let tb = release::fetch_tag_asset(&release.tag, release::MUSL_TARGET)?;
    verify_release(tb.path(), latest)?;
    client_ops::bootstrap_queue(t.as_ref(), name, queue, tb.path().to_str())?;
    if a.json {
        println!("{}", serde_json::json!({"action": "upgraded", "client": name, "queue": queue, "from": curver, "to": latest}));
    } else {
        println!("{name}/{queue}: upgraded {curver} → {latest}");
    }
    // Stage-then-mv replaced the binary FILE; a running daemon keeps
    // its old in-memory image — `shunt start` on a live daemon is a
    // no-op by design, so nothing else tells the operator. Restart
    // is their call (STOP drains in-flight tasks, never kills them),
    // but staleness must be loud or queue-semantics upgrades (new
    // directives, reschedule) silently don't apply.
    // Best-effort staleness hint: every failure below a parsed
    // report (transport, conn-down, drift) is silently ignored —
    // the upgrade already happened; this only advises the restart.
    if let remote::StatusAnswer::Report { report, .. } = remote::remote_status(t.as_ref(), &resolved) {
        if report.daemon.alive {
            let pid = report.daemon.pid.map(|p| format!(" (pid {p})")).unwrap_or_default();
            eprintln!("{name}/{queue}: daemon{pid} still runs the OLD binary image — `shunt queue stop {queue} -c {name}` drains and exits, then `shunt queue start {queue} -c {name}` brings the new one up");
        }
    }
    Ok(())
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
