//! `serve` lifecycle verbs (design.md §18): `up`/`down`/`restart`/
//! `status` and the pidfile + `/proc` cmdline identity they share,
//! plus the recovery of the running instance's flags from its argv —
//! the input `up`/`restart` converge against. Same discipline as the
//! queue daemon's §3.4: never pgrep, and never remove a pidfile
//! without proof of death.

use super::flags::{ServeFlags, ServeOpts, DEFAULT_BIND, DEFAULT_INTERVAL_S, DEFAULT_PORT};
use super::guard;
use anyhow::{Context as _, Result};
use serde_json::json;
use std::os::unix::process::CommandExt as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

/// Flag values recovered from a live `serve run` argv — what the
/// instance was actually started with. `None` marks a flag absent
/// from the argv (hand-typed `run`), which reads as the built-in
/// default everywhere it matters.
#[derive(Default)]
struct LiveFlags {
    bind: Option<String>,
    port: Option<u16>,
    token: Option<String>,
    interval_s: Option<u64>,
}

/// Pull `--bind/--port/--token/--interval-s` out of a `serve run`
/// argv — both `--flag v` (`up` generates this) and `--flag=v`.
fn live_flags(argv: &[&[u8]]) -> LiveFlags {
    let mut lf = LiveFlags::default();
    let mut i = 0;
    while i < argv.len() {
        let a = std::str::from_utf8(argv[i]).unwrap_or("");
        let (flag, inline) = match a.split_once('=') {
            Some((k, v)) => (k, Some(v.to_string())),
            None => (a, None),
        };
        if matches!(flag, "--bind" | "--port" | "--token" | "--interval-s") {
            let v = match inline {
                Some(v) => Some(v),
                None => {
                    i += 1;
                    argv.get(i).and_then(|e| std::str::from_utf8(e).ok()).map(str::to_string)
                }
            };
            match flag {
                "--bind" => lf.bind = v,
                "--port" => lf.port = v.and_then(|s| s.parse().ok()),
                "--interval-s" => lf.interval_s = v.and_then(|s| s.parse().ok()),
                _ => lf.token = v,
            }
        }
        i += 1;
    }
    lf
}

/// Explicitly-passed flags vs the live instance's argv — the diffs
/// a converge must not silently drop. Token diffs never print the
/// value.
fn flag_mismatches(explicit: &ServeFlags, live: &LiveFlags) -> Vec<String> {
    let mut out = vec![];
    if let Some(b) = &explicit.bind {
        let cur = live.bind.as_deref().unwrap_or(DEFAULT_BIND);
        if b != cur {
            out.push(format!("--bind {cur} → {b}"));
        }
    }
    if let Some(p) = explicit.port {
        let cur = live.port.unwrap_or(DEFAULT_PORT);
        if p != cur {
            out.push(format!("--port {cur} → {p}"));
        }
    }
    if let Some(i) = explicit.interval_s {
        let cur = live.interval_s.unwrap_or(DEFAULT_INTERVAL_S);
        if i != cur {
            out.push(format!("--interval-s {cur} → {i}"));
        }
    }
    if explicit.token.is_some() && explicit.token != live.token {
        out.push("--token differs".to_string());
    }
    out
}

/// The live instance's pid plus its argv-derived flags, or None.
/// Shared by the converge paths (`up`/`restart`) and `status`.
fn running_instance() -> Result<Option<(u32, LiveFlags)>> {
    let Some(pid) = alive()? else {
        return Ok(None);
    };
    let live = crate::proto::proc::read_cmdline(pid).ok().map(|raw| live_flags(&crate::proto::proc::split_argv(&raw))).unwrap_or_default();
    Ok(Some((pid, live)))
}

// ---- lifecycle (`serve up` / `serve down`) ----

pub(super) fn pid_file() -> Result<PathBuf> {
    Ok(crate::server::registry::shunt_home()?.join("serve.pid"))
}

fn log_file() -> Result<PathBuf> {
    Ok(crate::server::registry::shunt_home()?.join("serve.log"))
}

/// `$SHUNT_HOME/serve.ver` — the §3.5a capability gate, same role
/// as the queue daemon's `daemon.ver`: written only by an image
/// that HAS the USR2 handler, so `restart` can signal without
/// default-killing a pre-handler instance.
pub(super) fn ver_file() -> Result<PathBuf> {
    Ok(crate::server::registry::shunt_home()?.join("serve.ver"))
}

/// Serve identity for `proto::proc::probe`: shunt-family argv[0]
/// basename + argv[1] == `serve` + argv[2] == `run`. Anchoring the
/// leaf keeps a recycled pid from false-identifying (§3.4).
fn is_serve_cmdline(argv: &[&[u8]]) -> bool {
    let Some(argv0) = argv.first() else { return false };
    if !crate::proto::proc::is_shunt_argv0(argv0) {
        return false;
    }
    argv.get(1).is_some_and(|a| *a == b"serve") && argv.get(2).is_some_and(|a| *a == b"run")
}

/// Liveness: pidfile + /proc probe. Stale files are evicted only
/// when the pid is PROVEN dead and still names the same pid (a
/// rewritten file belongs to a live instance).
fn alive() -> Result<Option<u32>> {
    use crate::proto::proc::{self, ProcProbe};
    let pidf = pid_file()?;
    let pid: u32 = match std::fs::read_to_string(&pidf).ok().and_then(|s| s.trim().parse().ok()) {
        Some(p) => p,
        None => return Ok(None),
    };
    match proc::probe(pid, proc::read_cmdline, is_serve_cmdline) {
        ProcProbe::Live | ProcProbe::Unknown => Ok(Some(pid)),
        ProcProbe::Dead => {
            let still = std::fs::read_to_string(&pidf).ok().and_then(|s| s.trim().parse::<u32>().ok()) == Some(pid);
            if still {
                let _ = std::fs::remove_file(&pidf);
            }
            Ok(None)
        }
    }
}

/// `serve up`: detach `serve run …` as a background child — its
/// stdio lands in serve.log, its pid in serve.pid. Converges when
/// one is already up, but an explicitly-passed flag that differs
/// from the running argv is refused — converging would silently
/// drop the intent.
pub fn up(flags: &ServeFlags) -> Result<()> {
    if let Some((pid, live)) = running_instance()? {
        let miss = flag_mismatches(flags, &live);
        if miss.is_empty() {
            println!("already up (pid {pid} — http://{}:{})", live.bind.as_deref().unwrap_or(DEFAULT_BIND), live.port.unwrap_or(DEFAULT_PORT));
            return Ok(());
        }
        anyhow::bail!("already running (pid {pid}) on different flags: {} — `serve down` first to change them", miss.join(", "));
    }
    let opts: ServeOpts = flags.resolve();
    guard::ensure_bind_allowed(&opts)?;
    let dir = crate::server::registry::shunt_home()?;
    std::fs::create_dir_all(&dir)?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(log_file()?)?;
    let err = log.try_clone()?;
    // serve.log accumulates one banner per start — without a
    // delimiter a restart loop and a week of up/down cycles look
    // identical. Timestamp each launch.
    use std::io::Write as _;
    writeln!(&err, "--- serve up {} ({}:{}) ---", crate::proto::time::rfc3339_z_now(), opts.bind, opts.port).ok();
    // self_exe strips the ` (deleted)` suffix a self-replaced
    // caller's /proc/self/exe carries — same spawn trap as
    // daemon::start.
    let exe = crate::handoff::self_exe()?;
    let mut child = Command::new(exe);
    child.args(["serve", "run", "--bind", &opts.bind, "--port", &opts.port.to_string(), "--interval-s", &opts.interval_s.to_string()]);
    if let Some(t) = &opts.token {
        child.args(["--token", t]);
    }
    child.stdin(Stdio::null()).stdout(log).stderr(err).process_group(0);
    let mut spawned = child.spawn().context("cannot spawn serve")?;
    drop(spawned.stdin.take());
    for _ in 0..200 {
        if let Some(pid) = alive()? {
            println!("serve up: pid {pid} — http://{}:{} — logs: {} — `shunt serve down` stops", opts.bind, opts.port, log_file()?.display());
            return Ok(());
        }
        if let Ok(Some(_)) = spawned.try_wait() {
            if let Some(pid) = alive()? {
                println!("serve up: pid {pid} — http://{}:{} — logs: {} — `shunt serve down` stops", opts.bind, opts.port, log_file()?.display());
                return Ok(());
            }
            anyhow::bail!("serve exited during startup; check {}", log_file()?.display());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    anyhow::bail!("serve did not come up within 10 s; check {}", log_file()?.display())
}

/// `serve down`: verify the pidfile's identity, SIGTERM, let the
/// child's handler remove the pidfile (we remove too — same-file
/// NotFound is fine).
pub fn down() -> Result<()> {
    use crate::proto::proc::{self, ProcProbe};
    let pidf = pid_file()?;
    let Some(pid) = std::fs::read_to_string(&pidf).ok().and_then(|s| s.trim().parse::<u32>().ok()) else {
        println!("serve not running");
        return Ok(());
    };
    match proc::probe(pid, proc::read_cmdline, is_serve_cmdline) {
        ProcProbe::Dead => {
            let _ = std::fs::remove_file(&pidf);
            println!("serve not running (removed stale pidfile)");
        }
        ProcProbe::Live | ProcProbe::Unknown => {
            let rc = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
            if rc != 0 {
                return Err(std::io::Error::last_os_error()).with_context(|| format!("cannot SIGTERM serve pid {pid}"));
            }
            for _ in 0..100 {
                if unsafe { libc::kill(pid as i32, 0) } != 0 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = std::fs::remove_file(&pidf);
            let _ = std::fs::remove_file(ver_file()?);
            println!("serve down: stopped pid {pid}");
        }
    }
    Ok(())
}

/// The §3.5a signal half of `restart`: `serve.ver` gate + USR2 +
/// report on a live instance, `None` when nothing runs. `upgrade`
/// calls this directly — it must hand off a live serve but never
/// start one.
pub(crate) fn handoff_if_running() -> Result<Option<u32>> {
    let Some((pid, _)) = running_instance()? else {
        return Ok(None);
    };
    let ver = std::fs::read_to_string(ver_file()?).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let Some(from_ver) = ver else {
        anyhow::bail!("serve (pid {pid}) predates handoff support — no serve.ver, and SIGUSR2 would kill it; `shunt serve down` then `serve up` to move onto the new binary");
    };
    let rc = unsafe { libc::kill(pid as i32, libc::SIGUSR2) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| format!("cannot SIGUSR2 serve pid {pid}"));
    }
    println!("serve handed off (pid {pid}, was {from_ver}) — re-execs its own argv on the new image");
    Ok(Some(pid))
}

/// `serve restart`: converge on a running new-image instance.
/// Nothing live → fall through to `up` (`queue restart` → `start`
/// is the same posture). Live → SIGUSR2 handoff; explicit flags
/// warn rather than apply, because re-exec preserves the running
/// argv and only down+up can change them.
pub fn restart(flags: &ServeFlags) -> Result<()> {
    let Some((_pid, live)) = running_instance()? else {
        return up(flags);
    };
    let miss = flag_mismatches(flags, &live);
    if !miss.is_empty() {
        eprintln!("note: running instance keeps its own flags ({}); `serve down`+`serve up` to change them", miss.join(", "));
    }
    handoff_if_running().map(|_| ())
}

/// `serve status`: liveness, recorded version, endpoint parsed out
/// of the running argv, handoff capability (`serve.ver` present),
/// and the log path — one human line or `--json`.
pub fn status(json: bool) -> Result<()> {
    match running_instance()? {
        None => {
            if json {
                println!("{}", json!({"running": false}));
            } else {
                println!("serve: not running");
            }
        }
        Some((pid, live)) => {
            let ver = std::fs::read_to_string(ver_file()?).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
            let handoff = ver.is_some();
            let bind = live.bind.unwrap_or_else(|| DEFAULT_BIND.to_string());
            let port = live.port.unwrap_or(DEFAULT_PORT);
            if json {
                println!("{}", json!({"running": true, "pid": pid, "ver": ver, "bind": bind, "port": port, "handoff": handoff, "log": log_file()?.display().to_string()}));
            } else {
                let v = ver.unwrap_or_else(|| "unknown".into());
                let hf = if handoff { "handoff-capable" } else { "pre-handoff image — restart refuses; `serve down`+`serve up` to upgrade" };
                println!("serve up: pid {pid} — http://{bind}:{port} — {v} — {hf} — log {}", log_file()?.display());
            }
        }
    }
    Ok(())
}
