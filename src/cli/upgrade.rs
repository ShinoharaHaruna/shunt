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
    handoff_local_daemons();
    // The dashboard re-execs under the same §3.5a gate: a
    // pre-handler serve image is refused with the down+up
    // remediation rather than killed by an unhandled USR2. Signal-
    // only — a stopped serve must not be started by an upgrade.
    if let Err(e) = crate::server::web::handoff_if_running() {
        eprintln!("serve not handed off: {e:#}");
    }
    // The dispatcher has no pidfile (§16): the /proc scan is the
    // only discovery path, and the signal stays the operator's call —
    // a pre-handler image has no daemon.ver gate and USR2 would kill
    // it. Naming only pids that actually exist keeps the note honest.
    let dispatchers = running_dispatchers();
    if !dispatchers.is_empty() {
        eprintln!("note: running dispatcher(s) {} keep their old image — `kill -USR2 <pid>` re-execs in place, or restart", dispatchers.iter().map(u32::to_string).collect::<Vec<_>>().join(" "));
    }
    Ok(())
}

/// Live `shunt dispatch` processes owned by this uid — a /proc
/// cmdline scan, because the dispatcher keeps no pidfile
/// (design.md §16). Same-uid only: a foreign dispatcher cannot take
/// our signal, and naming it would mislead the operator.
fn running_dispatchers() -> Vec<u32> {
    use crate::proto::proc;
    use std::os::unix::fs::MetadataExt;
    let mut pids = Vec::new();
    let Ok(rd) = std::fs::read_dir("/proc") else { return pids };
    for ent in rd.flatten() {
        let Some(pid) = ent.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
        let Ok(raw) = proc::read_cmdline(pid) else { continue };
        let argv = proc::split_argv(&raw);
        let Some(argv0) = argv.first() else { continue };
        if !proc::is_shunt_argv0(argv0) || !is_dispatch_tail(&argv) {
            continue;
        }
        if std::fs::metadata(ent.path()).map(|m| m.uid()).unwrap_or(u32::MAX) != unsafe { libc::getuid() } {
            continue;
        }
        pids.push(pid);
    }
    pids
}

/// The dispatch needle: argv[1] == "dispatch" (argv[0] family is the
/// shared `is_shunt_argv0` check the caller applies first).
fn is_dispatch_tail(argv: &[&[u8]]) -> bool {
    argv.get(1).is_some_and(|a| *a == b"dispatch")
}

/// Hand off every live daemon registered on this box (§3.5a/§16):
/// `restart` SIGUSR2s it and the image re-execs its own path — the
/// stage-then-mv install already placed the new binary there. The
/// per-daemon `daemon.ver` gate means a pre-handler image is named
/// in the report, never killed by an unhandled signal. A daemon
/// started from a DIFFERENT binary than the upgraded `current_exe`
/// re-execs its own stale image — harmless but reported with its
/// `daemon.bin` path.
fn handoff_local_daemons() {
    let dir = crate::client::shutdown::registry_dir();
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return;
    };
    for ent in rd.flatten() {
        let Ok(qroot) = std::fs::read_to_string(ent.path()).map(|s| PathBuf::from(s.trim().to_string())) else {
            continue;
        };
        // A registered qroot that no longer exists is unambiguously
        // dead — the daemon re-registers on start — so sweep the
        // entry instead of letting restart's start-fallthrough
        // spawn into a ghost dir.
        if !qroot.is_dir() {
            let _ = std::fs::remove_file(ent.path());
            eprintln!("daemon at {} not handed off: qroot missing — swept its stale registry entry", qroot.display());
            continue;
        }
        match crate::client::daemon::restart(&qroot) {
            Ok(crate::client::daemon::Restart::HandedOff { pid, from_ver }) => println!("daemon at {} handed off (pid {pid}, was {from_ver})", qroot.display()),
            Ok(crate::client::daemon::Restart::Started { pid }) => println!("daemon at {} was dead; started fresh (pid {pid})", qroot.display()),
            Ok(crate::client::daemon::Restart::ExitedIdle) => println!("daemon at {} was dead; started and exited cleanly on idle (exit_on_idle)", qroot.display()),
            Err(e) => eprintln!("daemon at {} not handed off: {e:#}", qroot.display()),
        }
    }
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
    // A local row has no box binary: its daemon is spawned from the
    // dev machine's own image and re-execs self_exe() on restart, so
    // <qroot>/bin/shunt is never deployed. Smoking that missing path
    // misreports "compat error", and shipping the musl asset would
    // install an orphan nothing execs — `shunt upgrade` (no -c) is
    // the verb that upgrades the shared binary.
    if c.local {
        let cur = env!("CARGO_PKG_VERSION");
        let newer = release::semver_newer(latest, cur);
        if a.check || a.json {
            let j = serde_json::json!({"action": "check", "client": name, "queue": queue, "local": true, "current": cur, "latest": latest, "newer": newer});
            if a.json {
                println!("{j}");
                return Ok(());
            }
        }
        if a.check {
            let state = if newer { format!("new release available: {cur} → {latest} — `shunt upgrade` (no -c) upgrades it") } else { "up to date".into() };
            println!("{name}/{queue}: local row — daemon runs the dev machine's binary ({cur}); {state}");
            return Ok(());
        }
        println!("{name}/{queue}: local row — daemon runs the dev machine's binary ({cur}); nothing to upgrade — `shunt upgrade` (no -c) covers it");
        return Ok(());
    }
    let t = transport::transport_for(c);
    let bin = shunt_bin(c);
    // The remote --version doubles as the §15 compat smoke: loader
    // noise or a non-starting binary is itself a reason to relink
    // to the musl asset, even when the version already matches.
    let (smoke, cur) = match t.exec(c, &["sh".into(), "-c".into(), format!("{} --version 2>&1", shell_quote(&bin))], None) {
        Ok(out) => (compat::classify_smoke(&out), out),
        Err(e) => (compat::Compat::Error(format!("{e:#}")), String::new()),
    };
    let curver = release::parse_version(&cur).map(|(v, _, _)| v).unwrap_or_else(|| "?".into());
    let newer = release::semver_newer(latest, &curver);
    let relink = !matches!(smoke, compat::Compat::Ok);
    let compat_tag = match &smoke {
        compat::Compat::Ok => "ok",
        compat::Compat::Warn(_) => "warn",
        compat::Compat::Error(_) => "error",
    };
    if a.check || a.json {
        let j = serde_json::json!({"action": "check", "client": name, "queue": queue, "current": curver, "latest": latest, "newer": newer, "compat": compat_tag});
        if a.json {
            println!("{j}");
            return Ok(());
        }
    }
    if a.check {
        let state = if relink { format!(", compat {compat_tag}") } else { String::new() };
        println!("{name}/{queue}: {}{state}", if newer { format!("new release available: {curver} → {latest}") } else { format!("up to date ({curver})") });
        return Ok(());
    }
    if !newer && !relink && !a.force {
        println!("{name}/{queue}: up to date ({curver})");
        return Ok(());
    }
    if relink && !newer {
        match &smoke {
            compat::Compat::Warn(w) => eprintln!("{name}/{queue}: {w} — relinking to the musl release ({latest})"),
            compat::Compat::Error(e) => eprintln!("{name}/{queue}: box binary cannot run: {e} — replacing with the musl release ({latest})"),
            compat::Compat::Ok => {}
        }
    }
    anyhow::ensure!(t.exec(c, &["uname".into(), "-m".into()], None).map(|s| s.trim() == "x86_64").unwrap_or(false), "release assets are x86_64-only; {name} is not x86_64 — upgrade it by shipping a locally built binary: `client bootstrap {name} --bin-path <build>`");
    let tb = release::fetch_tag_asset(&release.tag, release::MUSL_TARGET)?;
    verify_release(tb.path(), latest)?;
    client_ops::bootstrap_queue(t.as_ref(), name, queue, tb.path().to_str())?;
    if a.json {
        println!("{}", serde_json::json!({"action": "upgraded", "client": name, "queue": queue, "from": curver, "to": latest, "relinked": relink && !newer}));
    } else if relink && !newer {
        println!("{name}/{queue}: relinked to musl {latest}");
    } else {
        println!("{name}/{queue}: upgraded {curver} → {latest}");
    }
    // The wire `restart` SIGUSR2s the live daemon — it re-execs
    // <qroot>/bin/shunt, exactly the path bootstrap installed over,
    // so queue-semantics changes take effect WITHOUT draining or
    // killing in-flight tasks (§3.5a). Verification is the daemon's
    // own `daemon.ver` flipping to the target, not "signal sent": a
    // pre-handler image refuses the op (the gate lives in daemon.ver
    // on the box, so an old image is never USR2-killed), and a
    // daemon started from another path re-execs its own stale image.
    let stale = |extra: &str| {
        let bin = match remote::remote_status(t.as_ref(), &resolved) {
            remote::StatusAnswer::Report { report, .. } => report.daemon.bin.map(|b| format!(" (running image: {b})")),
            _ => None,
        }
        .unwrap_or_default();
        eprintln!("{name}/{queue}: daemon not handed off{bin} — {extra}; run `shunt queue restart {queue} -c {name}` after fixing, or `queue stop`+`queue start`");
    };
    match remote::wire_call(t.as_ref(), &resolved, &["restart", "--json"], None) {
        remote::WireCall::Reply(_) => {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
            let mut flipped = false;
            // The handoff path polls through the daemon's own re-exec:
            // the fresh image rewrites daemon.pid/daemon.ver during
            // startup, and a status exec inside that window reads a
            // transient dead. Only a SUSTAINED dead streak proves the
            // handoff lost the daemon — §3.5a keeps the old image
            // running on exec failure, so one dead poll means nothing.
            let mut dead_streak = 0u32;
            while std::time::Instant::now() < deadline {
                if let remote::StatusAnswer::Report { report, .. } = remote::remote_status(t.as_ref(), &resolved) {
                    // daemon.ver is SHUNT_VERSION — "0.9.1 (<sha>
                    // <date>)", NO "shunt " prefix — so parse_version
                    // (built for `--version` output) returns None on
                    // it and a parse-based compare can never flip.
                    // The leading whitespace token IS the semver.
                    let flipped_now = report.daemon.alive && report.daemon.ver.as_deref().and_then(|v| v.split_whitespace().next()) == Some(latest);
                    if flipped_now {
                        flipped = true;
                        break;
                    }
                    dead_streak = if report.daemon.alive { 0 } else { dead_streak + 1 };
                    if dead_streak >= 3 {
                        break;
                    }
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            if flipped {
                println!("{name}/{queue}: daemon handed off to {latest}");
            } else {
                stale("restart was sent but daemon.ver did not reach the target");
            }
        }
        remote::WireCall::RemoteErr(e) => stale(&format!("restart refused: {e:#}")),
        remote::WireCall::ConnDown(e) => stale(&format!("connection down: {e:#}")),
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

#[cfg(test)]
mod tests {
    /// The /proc scan is the feature — and identity is the cmdline,
    /// not the executable (same rule as the §3.4 liveness needle):
    /// `exec -a shunt yes dispatch` makes a `yes` process read as
    /// `shunt dispatch` in /proc, which must appear in the scan and
    /// drop out again once killed.
    #[test]
    fn running_dispatchers_finds_only_live_dispatchers() {
        let mut child = std::process::Command::new("bash").args(["-c", "exec -a shunt yes dispatch >/dev/null"]).spawn().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !super::running_dispatchers().contains(&child.id()) {
            assert!(std::time::Instant::now() < deadline, "forged dispatcher pid {} never appeared in the scan", child.id());
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!super::running_dispatchers().contains(&child.id()), "a dead dispatcher must drop out of the scan");
    }
}
