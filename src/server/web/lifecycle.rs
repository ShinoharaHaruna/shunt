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
    spawn_up(&flags.resolve())
}

/// The spawn-and-wait half of `up`, shared with `restart`: a restart
/// has just proved nothing is live, so it skips the `up` converge.
fn spawn_up(opts: &ServeOpts) -> Result<()> {
    guard::ensure_bind_allowed(opts)?;
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
            // A ≤0.10 image wrote this USR2-capability marker. Nothing
            // reads it since restart became a hard down+up, and leaving
            // it behind would advertise a mechanism that no longer
            // exists.
            if let Ok(home) = crate::server::registry::shunt_home() {
                let _ = std::fs::remove_file(home.join("serve.ver"));
            }
            println!("serve down: stopped pid {pid}");
        }
    }
    Ok(())
}

/// Effective options for a restart: explicit flags win, then the
/// live instance's own argv, then the built-in defaults. Recovered
/// from argv rather than remembered — a hard restart spawns a fresh
/// process, so nothing carries over implicitly.
fn restart_opts(flags: &ServeFlags, live: &LiveFlags) -> ServeOpts {
    ServeOpts { bind: flags.bind.clone().or_else(|| live.bind.clone()).unwrap_or_else(|| DEFAULT_BIND.to_string()), port: flags.port.or(live.port).unwrap_or(DEFAULT_PORT), token: flags.token.clone().or_else(|| live.token.clone()), interval_s: flags.interval_s.or(live.interval_s).unwrap_or(DEFAULT_INTERVAL_S) }
}

/// `serve restart`: a hard down+up on the installed image. Serve
/// holds only the merged snapshot (§18) — every section is recomputed
/// on the next sweep and the §4.1 probe penalty lives in the registry,
/// not in the process — so unlike the daemon (§3.5a) it has no
/// in-flight work a signal handoff would protect. What it buys: no
/// capability gate, so a restart works on ANY running image, and an
/// exec can never leave a stale image running while claiming success.
/// Flags: explicit ones win over the live argv, which a down+up CAN
/// change.
pub fn restart(flags: &ServeFlags) -> Result<()> {
    match running_instance()? {
        None => up(flags),
        Some((pid, live)) => restart_live(flags, pid, &live),
    }
}

/// `upgrade`'s entry (§16): restart a live serve onto the new image,
/// never start one that is stopped — an upgrade is not a start. The
/// bool reports whether an instance was there to restart.
pub(crate) fn restart_if_running() -> Result<bool> {
    match running_instance()? {
        None => Ok(false),
        Some((pid, live)) => restart_live(&ServeFlags::default(), pid, &live).map(|()| true),
    }
}

fn restart_live(flags: &ServeFlags, pid: u32, live: &LiveFlags) -> Result<()> {
    let opts = restart_opts(flags, live);
    println!("serve restart: stopping pid {pid} — down+up on {}:{}", opts.bind, opts.port);
    down()?;
    spawn_up(&opts)
}

/// `serve status`: liveness, the endpoint parsed out of the running
/// argv, and the log path — one human line or `--json`. Deliberately
/// no version field: a hard restart means the running image IS the
/// installed one, so a recorded version would only be able to lie.
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
            let bind = live.bind.unwrap_or_else(|| DEFAULT_BIND.to_string());
            let port = live.port.unwrap_or(DEFAULT_PORT);
            if json {
                println!("{}", json!({"running": true, "pid": pid, "bind": bind, "port": port, "log": log_file()?.display().to_string()}));
            } else {
                println!("serve up: pid {pid} — http://{bind}:{port} — log {}", log_file()?.display());
            }
        }
    }
    Ok(())
}
