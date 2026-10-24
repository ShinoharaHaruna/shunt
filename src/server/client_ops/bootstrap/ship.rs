//! Per-queue binary ship: the §4.1 owner claim, then stage →
//! smoke → §15 classify → install by the ladder ok > warn >
//! refuse (with the musl release rescue), then init, env push,
//! and the live-daemon SIGUSR2 handoff or cold start.

use super::claim::{owner_path, owner_pubkey, CLAIM_SCRIPT};
use crate::server::client_ops::compat::Compat;
use crate::server::client_ops::env::push_resolved_env;
use crate::server::client_ops::{compat, release};
use crate::server::registry::ResolvedQueue;
use crate::server::remote;
use crate::server::transport::{shell_quote, shunt_bin, Transport};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// The musl build recipe named in every incompatibility error.
const MUSL_RECIPE: &str = "rustup target add x86_64-unknown-linux-musl && cargo build --release --target x86_64-unknown-linux-musl";

pub(super) fn bootstrap_resolved(t: &dyn Transport, queue: &ResolvedQueue, bin_path: Option<&str>) -> Result<()> {
    let entry = &queue.client;
    let name = &entry.name;
    // .shunt-owner claim (§4.1): two independent shunt installs
    // pointed at the same qroot silently merge into one queue —
    // seq races between two dispatchers, one daemon serving both.
    // One atomic remote script, BEFORE any write: `set -C` gives
    // create-exclusive so a simultaneous claim cannot interleave —
    // a later bootstrap can never preempt an earlier owner's qroot.
    if let Some(pubkey) = owner_pubkey(entry) {
        let owner = owner_path(entry);
        let verdict = t.exec(entry, &["sh".into(), "-c".into(), CLAIM_SCRIPT.into(), owner, pubkey], None).context("claim .shunt-owner")?;
        let verdict = verdict.trim();
        if verdict == "FOREIGN" {
            anyhow::bail!("qroot {} is claimed by a different shunt install (.shunt-owner mismatch) — pick a different qroot in the row; if the other install is gone, remove {}", entry.qroot, owner_path(entry));
        }
        anyhow::ensure!(matches!(verdict, "OWNED" | "CLAIMED"), "unexpected .shunt-owner claim reply: {verdict:?}");
    }
    let bin = shunt_bin(entry);
    let bindir = bin.rsplit_once('/').map(|(d, _)| d).unwrap_or("").to_string();
    t.exec(entry, &["mkdir".into(), "-p".into(), bindir.clone()], None).context("mkdir remote bin dir")?;

    // §15: pick the binary, then let the box say what it does —
    // stage → `<staged> --version 2>&1` → three-state classify →
    // install by the ladder ok > warn > refuse. Observed behavior
    // covers every failure class (version refs, arch, interpreter),
    // not just the glibc mismatch it was built for.
    let explicit = bin_path.map(PathBuf::from).or_else(|| entry.bin_path.as_deref().map(PathBuf::from));
    let local = match &explicit {
        Some(p) => {
            anyhow::ensure!(p.is_file(), "--bin-path/bin_path {}: no such file", p.display());
            p.clone()
        }
        None => std::env::current_exe().context("current_exe")?,
    };
    // Diagnostics only: the byte-scan names the glibc need in
    // reports and gates the musl rescue (a static binary has no
    // release to swap for). It never decides.
    let needs = compat::required_glibc(&local)?;

    // Pre-install liveness: bootstrap is the manual redeploy path —
    // a live daemon must hand off to the new image (§3.5a restart),
    // not be left running the stale one after a silent `start` no-op.
    let was_live = matches!(remote::remote_status(t, queue), remote::StatusAnswer::Report { report, .. } if report.daemon.alive);

    let staged = format!("{bin}.new");
    // Stage one candidate and smoke it. The staged path — never the
    // install path — is what runs, so a bad binary cannot land.
    let try_binary = |path: &Path| -> Result<Compat> {
        t.copy_to(entry, path, &staged).context("copy shunt binary")?;
        t.exec(entry, &["chmod".into(), "+x".into(), staged.clone()], None)?;
        Ok(match t.exec(entry, &["sh".into(), "-c".into(), format!("{} --version 2>&1", shell_quote(&staged))], None) {
            Ok(out) => compat::classify_smoke(&out),
            Err(e) => Compat::Error(format!("{e:#}")),
        })
    };
    let drop_staged = || {
        let _ = t.exec(entry, &["rm".into(), "-f".into(), staged.clone()], None);
    };
    // "needs GLIBC 2.39; box has glibc 2.35" — context for reports.
    let diag = |t: &dyn Transport| -> String {
        let mut d = needs.map(|(a, b)| format!("needs GLIBC {a}.{b}")).unwrap_or_else(|| "libc-independent".into());
        if let Some(l) = compat::remote_libc(t, entry) {
            d.push_str(&format!("; box has {l}"));
        }
        d
    };

    let mut ship = local.clone();
    let mut _rescue = None; // keeps the downloaded TempBin alive until copy_to ran
    match try_binary(&local)? {
        Compat::Ok => {}
        Compat::Warn(w) => {
            // Prefer ok: on the auto source a matching musl release
            // upgrades warn → ok. Rescue is pointless for a static
            // binary (needs == None) and never substitutes an
            // operator-chosen explicit path.
            let rescued = if explicit.is_none() && needs.is_some() {
                match release::rescue_musl(&local) {
                    Ok(tb) => match try_binary(tb.path())? {
                        Compat::Ok => {
                            eprintln!("note: {}: {w} — shipping the matching musl release build", diag(t));
                            ship = tb.path().to_path_buf();
                            _rescue = Some(tb);
                            true
                        }
                        bad => {
                            drop_staged();
                            anyhow::bail!("{w} ({}); the musl release also fails on {name}: {bad:?}", diag(t))
                        }
                    },
                    Err(_) => false, // no same-code release exists — the warn fallback below applies
                }
            } else {
                false
            };
            if !rescued {
                let who = if explicit.is_some() {
                    "explicit --bin-path honored"
                } else if needs.is_none() {
                    "libc-independent binary — nothing to substitute"
                } else {
                    "no matching musl release — shipping anyway"
                };
                eprintln!("warning ({name}): {w} ({}); {who} — the noise recurs on every remote call and lands in daemon.log. Fix: {MUSL_RECIPE} → `client bootstrap {name} --bin-path <build>`, or `shunt upgrade -c {name}`", diag(t));
            }
        }
        Compat::Error(e) => {
            let d = diag(t);
            if explicit.is_some() {
                drop_staged();
                anyhow::bail!("{} cannot run on {name}: {e} ({d}) — ship a libc-independent build: {MUSL_RECIPE} → `client bootstrap {name} --bin-path <build>`", local.display());
            }
            match needs.is_some().then(|| release::rescue_musl(&local)) {
                Some(Ok(tb)) => match try_binary(tb.path())? {
                    Compat::Ok => {
                        eprintln!("note: binary cannot run on {name}: {e} ({d}) — shipping the matching musl release build");
                        ship = tb.path().to_path_buf();
                        _rescue = Some(tb);
                    }
                    bad => {
                        drop_staged();
                        anyhow::bail!("{e} ({d}); the musl release also fails on {name}: {bad:?}")
                    }
                },
                Some(Err(e2)) => {
                    drop_staged();
                    anyhow::bail!("{e} ({d}); release rescue failed ({e2:#}). Build one: {MUSL_RECIPE} → `client bootstrap {name} --bin-path target/x86_64-unknown-linux-musl/release/shunt`")
                }
                // A libc-independent binary that cannot run points at
                // arch/interp, not libc — rescue could only repeat it.
                None => {
                    drop_staged();
                    anyhow::bail!("{e} — libc-independent binary fails on {name} (arch/interp mismatch?); cannot rescue with another static build")
                }
            }
        }
    }
    if explicit.is_some() {
        // bin_path persists across re-bootstraps (the upgrade path) —
        // print what's being shipped vs what we run so drift is loud.
        let v = |p: &PathBuf| std::process::Command::new(p).arg("--version").output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_else(|| "?".into());
        eprintln!("ship {} ({})", ship.display(), v(&ship));
    }

    // Stage-then-mv: scp truncates the destination in place, which
    // fails ETXTBSY while the daemon runs the old binary. `staged`
    // already holds the classified winner — `try_binary` always
    // leaves its candidate there and failures bail before this.
    t.exec(entry, &["mv".into(), "-f".into(), staged, bin.clone()], None).context("install shunt binary")?;

    // Post-install smoke (§15): `exec` returns stdout only, so the
    // loader's stderr (weak-version warnings) is merged in via 2>&1.
    let smoke = t.exec(entry, &["sh".into(), "-c".into(), format!("{} --version 2>&1", shell_quote(&bin))], None).context("shipped binary cannot run on the box")?;
    for line in smoke.lines() {
        if line.trim_start().starts_with("shunt ") {
            eprintln!("remote: {}", line.trim());
        } else if !line.trim().is_empty() {
            eprintln!("warning (box): {line}");
        }
    }

    t.exec(entry, &[bin.clone(), "init".into(), "--root".into(), entry.qroot.clone(), "--json".into()], None).context("remote shunt init")?;
    // Apply residency and worker preferences before the daemon can claim its first task.
    // Empty legacy preferences preserve box-side edits during bootstrap.
    if !entry.env_setup.is_empty() || !entry.shell.is_empty() || entry.exit_on_idle || queue.workers.is_some() {
        push_resolved_env(t, queue)?;
    } else {
        crate::server::tunnel::sync_queue_env(t, &entry.name, &queue.name)?;
    }
    if was_live {
        // Redeploy on a live box: SIGUSR2 re-execs the daemon onto
        // the binary just installed — no drain, no task kill (§3.5a).
        // Refusals (a pre-handoff image has no daemon.ver) surface
        // named, never silently skipped.
        match remote::wire_call(t, queue, &["restart", "--json"], None) {
            remote::WireCall::Reply(_) => eprintln!("daemon handed off to the new binary (SIGUSR2)"),
            remote::WireCall::RemoteErr(e) => eprintln!("warning: daemon not handed off: {e:#} — `shunt queue restart {} -c {name}`, or `queue stop`+`queue start`", queue.name),
            remote::WireCall::ConnDown(e) => eprintln!("warning: daemon not handed off: {e:#} — retry `shunt queue restart {} -c {name}`", queue.name),
        }
    } else {
        t.exec(entry, &[bin.clone(), "start".into(), "--root".into(), entry.qroot.clone(), "--json".into()], None).context("remote shunt start")?;
    }
    eprintln!("bootstrapped {} queue {} ({})", entry.name, queue.name, bin);
    Ok(())
}
