//! `shunt serve` (design.md §18): the read-only ops dashboard on the
//! dev machine. One `TcpListener` serves the embedded SPA, a small
//! `/api/` surface, and the `/ws` fan-out; two broadcast loops merge
//! into one snapshot — the same truth `status --all` computes, for
//! a browser instead of a terminal.

mod assets;
mod http;
mod snapshot;
mod ws;

use anyhow::{Context as _, Result};
use serde_json::{json, Value};
use std::net::{TcpListener, TcpStream};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// The merged latest snapshot: broadcast loops update their own
/// section; every new conn gets it whole (ws channel head) or via
/// `/api/snapshot`.
type Latest = Arc<RwLock<Value>>;

pub struct ServeOpts {
    /// Listen address (default 127.0.0.1).
    pub bind: String,
    pub port: u16,
    /// Required once `bind` leaves loopback; checked as `?token=`,
    /// `x-shunt-token`, or `Authorization: Bearer`.
    pub token: Option<String>,
    /// Wire-probe cadence in seconds (the expensive half — one ssh
    /// exec per queue per tick).
    pub interval_s: u64,
}

pub const DEFAULT_BIND: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 8747;
pub const DEFAULT_INTERVAL_S: u64 = 10;

/// `run`/`up`/`restart` flags. All-Option fields: `None` = not
/// passed on the command line, so `up`/`restart` can tell an
/// explicit flag from a built-in default when converging against
/// a live instance's own argv.
#[derive(clap::Args, Clone)]
pub struct ServeFlags {
    /// Listen address (default 127.0.0.1 — reach a remote dev
    /// machine with `ssh -L PORT:127.0.0.1:PORT <dev>`)
    #[arg(long)]
    pub bind: Option<String>,
    /// Listen port (default 8747)
    #[arg(long)]
    pub port: Option<u16>,
    /// Access token — REQUIRED when --bind is not loopback. Checked
    /// as ?token=, the x-shunt-token header, or Authorization: Bearer
    #[arg(long)]
    pub token: Option<String>,
    /// Wire-probe cadence in seconds for the clients section (one
    /// ssh exec per queue per tick); local state updates every 2s
    /// (default 10)
    #[arg(long)]
    pub interval_s: Option<u64>,
}

impl ServeFlags {
    /// Effective options for actually launching: explicit flags
    /// over the built-in defaults. The converge paths in
    /// `up`/`restart` compare the explicit subset against the
    /// running argv instead.
    pub fn resolve(&self) -> ServeOpts {
        ServeOpts { bind: self.bind.clone().unwrap_or_else(|| DEFAULT_BIND.to_string()), port: self.port.unwrap_or(DEFAULT_PORT), token: self.token.clone(), interval_s: self.interval_s.unwrap_or(DEFAULT_INTERVAL_S) }
    }
}

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
// Same discipline as the queue daemon's §3.4: a pidfile under
// $SHUNT_HOME + a /proc cmdline identity check, never pgrep; the
// pidfile is removed only on proof of death.

fn pid_file() -> Result<PathBuf> {
    Ok(crate::server::registry::shunt_home()?.join("serve.pid"))
}

fn log_file() -> Result<PathBuf> {
    Ok(crate::server::registry::shunt_home()?.join("serve.log"))
}

/// `$SHUNT_HOME/serve.ver` — the §3.5a capability gate, same role
/// as the queue daemon's `daemon.ver`: written only by an image
/// that HAS the USR2 handler, so `restart` can signal without
/// default-killing a pre-handler instance.
fn ver_file() -> Result<PathBuf> {
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

fn is_loopback_bind(bind: &str) -> bool {
    matches!(bind, "127.0.0.1" | "127.0.1.1" | "::1" | "localhost")
}

fn ensure_bind_allowed(opts: &ServeOpts) -> Result<()> {
    let loopback = is_loopback_bind(&opts.bind);
    anyhow::ensure!(loopback || opts.token.is_some(), "refusing to bind {}:{} without --token — the dashboard exposes fleet state to anyone who can reach the port; bind 127.0.0.1 (and use `ssh -L {0}:{1}:127.0.0.1:{1} <dev>` from your browser) or pass --token", opts.bind, opts.port);
    Ok(())
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
    let opts = flags.resolve();
    ensure_bind_allowed(&opts)?;
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

/// Server-local sections refresh at this fixed cadence: fs reads
/// cost nothing next to a wire probe, so it is a constant.
const LOCAL_TICK_S: u64 = 2;

/// `/api/room/<name>` returns at most this many messages (newest of
/// the merged live+archive stream).
const ROOM_MSG_TAIL: usize = 200;

pub fn run(opts: ServeOpts) -> Result<()> {
    ensure_bind_allowed(&opts)?;

    let latest: Latest = Arc::new(RwLock::new(json!({
        "type": "snapshot",
        "protocol": crate::proto::wire::PROTOCOL_VERSION,
        "server": env!("SHUNT_VERSION"),
        "clients": Value::Null,
        "local": Value::Null,
        "ts": Value::Null,
    })));
    let hub = ws::Hub::new();

    // Slow sweep: wire probes at --interval. Clients stay null in
    // the snapshot until the first sweep lands — the UI shows local
    // state instead of an empty fleet.
    {
        let (latest, hub) = (latest.clone(), hub.clone());
        let interval = opts.interval_s.max(2);
        std::thread::spawn(move || loop {
            let clients = snapshot::clients_section();
            publish(&latest, &hub, |s| s["clients"] = clients.clone());
            std::thread::sleep(Duration::from_secs(interval));
        });
    }
    // Fast sweep: server-local fs state.
    {
        let (latest, hub) = (latest.clone(), hub.clone());
        std::thread::spawn(move || loop {
            publish(&latest, &hub, |s| s["local"] = snapshot::local_section());
            std::thread::sleep(Duration::from_secs(LOCAL_TICK_S));
        });
    }

    let listener = TcpListener::bind((opts.bind.as_str(), opts.port))?;
    let token_note = if opts.token.is_some() { " (append ?token=… to the URL)" } else { "" };
    println!("shunt serve: http://{}:{}{token_note} — read-only dashboard (fixed probes only); Ctrl-C stops", opts.bind, opts.port);

    // Lifecycle bookkeeping: pidfile first (up/down find us through
    // it), then a signal thread that removes it and exits — TERM
    // and INT both count as a clean stop.
    let pidf = pid_file()?;
    if let Some(dir) = pidf.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    std::fs::write(&pidf, format!("{}", std::process::id()))?;
    let verf = ver_file()?;
    std::fs::write(&verf, format!("{}\n", env!("SHUNT_VERSION")))?;
    // TERM/INT remove the lifecycle files and exit. USR2 re-execs
    // this image in place (§3.5a): the listener is CLOEXEC, so
    // execve drops it and the new image rebinds the same port — a
    // sub-second refusal window plus one dropped WS conn, never a
    // drain. An exec failure keeps the old image running.
    {
        let verf = verf.clone();
        let argv_rest: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
        let mut sigs = signal_hook::iterator::Signals::new([signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT, signal_hook::consts::SIGUSR2]).context("cannot install signal handlers")?;
        std::thread::spawn(move || {
            for sig in sigs.forever() {
                if sig == signal_hook::consts::SIGUSR2 {
                    eprintln!("serve: SIGUSR2 — re-exec in place");
                    let Err(e) = crate::handoff::exec_self(&argv_rest);
                    eprintln!("serve: handoff failed: {e} — continuing on the old image");
                } else {
                    let _ = std::fs::remove_file(&pidf);
                    let _ = std::fs::remove_file(&verf);
                    std::process::exit(0);
                }
            }
        });
    }
    crate::handoff::unblock_usr2();
    if std::env::var_os(crate::handoff::HANDOFF_ENV).is_some() {
        std::env::remove_var(crate::handoff::HANDOFF_ENV);
        eprintln!("serve: re-exec'd via handoff");
    }

    let auth = Arc::new(opts.token);
    let bind = opts.bind;
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let (latest, hub, auth, bind) = (latest.clone(), hub.clone(), auth.clone(), bind.clone());
                std::thread::spawn(move || {
                    let _ = handle(stream, &latest, &hub, &auth, &bind);
                });
            }
            Err(e) => eprintln!("serve: accept failed: {e}"),
        }
    }
    Ok(())
}

/// Merge one section in and push the whole snapshot — the payload
/// is a few KB, so a full resend beats diffing.
fn publish(latest: &Latest, hub: &ws::Hub, f: impl FnOnce(&mut Value)) {
    let text = {
        let mut g = latest.write().unwrap();
        f(&mut g);
        g["ts"] = json!(crate::proto::time::rfc3339_z_now());
        serde_json::to_string(&*g).unwrap_or_default()
    };
    hub.broadcast(&text);
}

fn host_component(host: &str) -> Option<&str> {
    let host = host.trim();
    if let Some(rest) = host.strip_prefix('[') {
        let (component, suffix) = rest.split_once(']')?;
        component.parse::<std::net::Ipv6Addr>().ok()?;
        if !suffix.is_empty() && !suffix.strip_prefix(':').is_some_and(|port| port.parse::<u16>().is_ok()) {
            return None;
        }
        return Some(component);
    }
    match host.rsplit_once(':') {
        Some((component, port)) if !component.contains(':') => {
            port.parse::<u16>().ok()?;
            Some(component)
        }
        Some(_) => None,
        None => Some(host),
    }
}

/// A loopback listener must not accept an attacker-controlled Host:
/// DNS rebinding keeps Host and Origin equal while resolving to 127.0.0.1.
fn loopback_host_allowed(host: Option<&str>, bind: &str) -> bool {
    let Some(host) = host.and_then(host_component) else { return false };
    let host = host.strip_suffix('.').unwrap_or(host);
    ["127.0.0.1", "localhost", "::1", bind].iter().any(|allowed| host.eq_ignore_ascii_case(allowed))
}

fn origin_authority(origin: &str) -> Option<&str> {
    let origin = origin.trim();
    if origin.eq_ignore_ascii_case("null") {
        return None;
    }
    let (scheme, authority) = origin.split_once("://")?;
    if !(scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")) || authority.is_empty() || authority.bytes().any(|b| b.is_ascii_whitespace() || matches!(b, b'/' | b'?' | b'#' | b'@' | b',')) {
        return None;
    }
    Some(authority)
}

/// Browser metadata blocks cross-site reads and writes before routing
/// or token checks; absent headers keep non-browser clients usable.
fn same_origin_ok(req: &http::Request) -> bool {
    if req.header("sec-fetch-site").is_some_and(|site| !site.eq_ignore_ascii_case("same-origin") && !site.eq_ignore_ascii_case("none")) {
        return false;
    }
    let Some(origin) = req.header("origin") else {
        return true;
    };
    let Some(host) = req.header("host") else {
        return false;
    };
    origin_authority(origin).is_some_and(|authority| authority.eq_ignore_ascii_case(host))
}

#[derive(serde::Deserialize)]
struct ProbeRequest {
    client: String,
    probe: String,
}

fn authorized(req: &http::Request, token: &Option<String>) -> bool {
    let Some(expected) = token else {
        return true;
    };
    let presented = http::query_param(&req.query, "token").or_else(|| req.header("x-shunt-token").map(str::to_string)).or_else(|| req.header("authorization").and_then(|a| a.strip_prefix("Bearer ").map(str::to_string)));
    presented.as_deref() == Some(expected.as_str())
}

fn handle(mut stream: TcpStream, latest: &Latest, hub: &ws::Hub, token: &Option<String>, bind: &str) -> Result<()> {
    stream.set_nodelay(true).ok();
    let Some(req) = http::read_request(&mut stream) else {
        return Ok(());
    };
    if is_loopback_bind(bind) && !loopback_host_allowed(req.header("host"), bind) {
        return http::json(&mut stream, 403, &json!({"error": "invalid Host for loopback bind"}));
    }
    if !same_origin_ok(&req) {
        return http::json(&mut stream, 403, &json!({"error": "cross-origin request rejected"}));
    }
    if !authorized(&req, token) {
        return http::json(&mut stream, 401, &json!({"error": "unauthorized — pass ?token=, x-shunt-token, or Authorization: Bearer"}));
    }
    if req.method == "POST" && req.path == "/api/probe" {
        // §18 exposes fixed inspection commands only; extra JSON
        // fields are ignored and cannot alter the selected argv.
        let body: ProbeRequest = match serde_json::from_slice(&req.body) {
            Ok(v) => v,
            Err(_) => return http::json(&mut stream, 400, &json!({"error": "body must be JSON {client, probe}"})),
        };
        if body.client.is_empty() || body.probe.is_empty() {
            return http::json(&mut stream, 400, &json!({"error": "expected {client, probe}"}));
        }
        if snapshot::probe_argv(&body.probe).is_none() {
            return http::json(&mut stream, 400, &json!({"error": format!("unknown probe {:?}; valid probes: {}", body.probe, snapshot::PROBE_NAMES.join(", "))}));
        }
        return match snapshot::probe(&body.client, &body.probe) {
            Ok(out) => http::json(&mut stream, 200, &json!({"ok": true, "out": out})),
            Err(e) => http::json(&mut stream, 200, &json!({"ok": false, "error": format!("{e:#}")})),
        };
    }
    if req.method != "GET" {
        return http::json(&mut stream, 405, &json!({"error": "GET only (POST /api/probe is the one probe route)"}));
    }
    match req.path.as_str() {
        "/ws" => ws::upgrade(stream, &req, hub.clone(), latest),
        "/api/snapshot" => http::json(&mut stream, 200, &latest.read().unwrap()),
        p if p.starts_with("/api/room/") => {
            let name = p.trim_start_matches("/api/room/");
            match snapshot::room_detail(name, ROOM_MSG_TAIL) {
                Ok(v) => http::json(&mut stream, 200, &v),
                Err(e) => http::json(&mut stream, 404, &json!({"error": format!("{e:#}")})),
            }
        }
        p if p.starts_with("/api/logs/") => {
            // /api/logs/<client>/<queue>?pattern=<pat>&tail=N —
            // queue may be empty (single-queue client).
            let segs: Vec<&str> = p.trim_start_matches("/api/logs/").split('/').collect();
            let client = segs.first().copied().unwrap_or("");
            let queue = segs.get(1).copied().filter(|q| !q.is_empty());
            let pattern = http::query_param(&req.query, "pattern");
            let tail = http::query_param(&req.query, "tail").and_then(|t| t.parse::<usize>().ok()).unwrap_or(200).min(2000);
            match snapshot::logs(client, queue, pattern.as_deref(), tail) {
                Ok(text) => http::json(&mut stream, 200, &json!({"ok": true, "text": text})),
                Err(e) => http::json(&mut stream, 200, &json!({"ok": false, "error": format!("{e:#}")})),
            }
        }
        p if p.starts_with("/api/script/") => {
            // /api/script/<client>/<queue>?name=<entry> — the entry's
            // task.sh verbatim; queue may be empty (single-queue
            // client). Same text-op shape as /api/logs.
            let segs: Vec<&str> = p.trim_start_matches("/api/script/").split('/').collect();
            let client = segs.first().copied().unwrap_or("");
            let queue = segs.get(1).copied().filter(|q| !q.is_empty());
            let name = http::query_param(&req.query, "name").unwrap_or_default();
            match snapshot::script(client, queue, &name) {
                Ok(text) => http::json(&mut stream, 200, &json!({"ok": true, "text": text})),
                Err(e) => http::json(&mut stream, 200, &json!({"ok": false, "error": format!("{e:#}")})),
            }
        }
        p => match assets::lookup(p) {
            Some((bytes, ctype)) => {
                // Vite emits hashed filenames under assets/ — safe to
                // cache forever; index.html must always revalidate.
                let cache = if p.starts_with("/assets/") { "public, max-age=31536000, immutable" } else { "no-cache" };
                // script-src carries the sha256 of index.html's
                // inline theme-init script (the only inline script
                // shipped) — edit that script, recompute its sha256.
                http::respond(&mut stream, 200, "OK", ctype, bytes, &[("cache-control", cache), ("content-security-policy", "default-src 'self'; script-src 'self' 'sha256-jX/bClGWvhh+f5nuFSHuWzYPMxe/COBXGImDlm5JWxc='")])
            }
            None => http::json(&mut stream, 404, &json!({"error": format!("not found: {p}")})),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(headers: &[(&str, &str)]) -> http::Request {
        http::Request { method: "GET".into(), path: "/".into(), query: String::new(), headers: headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(), body: Vec::new() }
    }

    #[test]
    fn same_origin_gate_accepts_same_origin_and_non_browser_requests() {
        assert!(same_origin_ok(&request(&[("host", "LOCALHOST:8747"), ("origin", "http://localhost:8747")])));
        assert!(same_origin_ok(&request(&[("sec-fetch-site", "none")])));
        assert!(same_origin_ok(&request(&[])));
    }

    #[test]
    fn same_origin_gate_rejects_cross_site_metadata_and_origins() {
        assert!(!same_origin_ok(&request(&[("sec-fetch-site", "cross-site")])));
        assert!(!same_origin_ok(&request(&[("sec-fetch-site", "same-site")])));
        assert!(!same_origin_ok(&request(&[("host", "localhost:8747"), ("origin", "null")])));
        assert!(!same_origin_ok(&request(&[("host", "localhost:8747"), ("origin", "http://evil.example")])));
    }

    #[test]
    fn loopback_host_gate_accepts_local_names_and_rejects_rebinding() {
        for host in ["127.0.0.1", "127.0.0.1:8747", "localhost", "LOCALHOST.:8747", "[::1]", "[::1]:8747"] {
            assert!(loopback_host_allowed(Some(host), "127.0.0.1"), "{host}");
        }
        assert!(loopback_host_allowed(Some("127.0.1.1:8747"), "127.0.1.1"));
        assert!(!loopback_host_allowed(Some("attacker.example:8747"), "127.0.0.1"));
        assert!(!loopback_host_allowed(Some("localhost.example:8747"), "127.0.0.1"));
        assert!(!loopback_host_allowed(None, "127.0.0.1"));
    }

    fn response_for(raw: &str, bind: &str) -> String {
        use std::io::{Read, Write};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut client = TcpStream::connect(addr).unwrap();
        client.write_all(raw.as_bytes()).unwrap();
        let (server, _) = listener.accept().unwrap();
        let latest: Latest = Arc::new(RwLock::new(json!({})));
        handle(server, &latest, &ws::Hub::new(), &None, bind).unwrap();

        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        response
    }

    #[test]
    fn loopback_request_rejects_rebinding_host_before_routing() {
        let response = response_for("GET /api/snapshot HTTP/1.1\r\nHost: attacker.example:8747\r\nOrigin: http://attacker.example:8747\r\nSec-Fetch-Site: same-origin\r\n\r\n", "127.0.0.1");
        assert!(response.starts_with("HTTP/1.1 403 Forbidden\r\n"));
        assert!(response.contains("invalid Host for loopback bind"));
    }

    #[test]
    fn probe_request_ignores_untrusted_command_fields() {
        let body: ProbeRequest = serde_json::from_str(r#"{"client":"localhost","probe":"ps","cmd":"rm -rf /"}"#).unwrap();
        assert_eq!(body.client, "localhost");
        assert_eq!(snapshot::probe_argv(&body.probe), Some(&["ps", "-eo", "pid,pgid,etime,%cpu,stat,cmd", "--sort=-%cpu"][..]));
        assert_eq!(snapshot::probe_argv("rm -rf /"), None);
    }

    #[test]
    fn unknown_probe_is_a_bad_request_listing_valid_names() {
        let body = r#"{"client":"localhost","probe":"unknown"}"#;
        let raw = format!("POST /api/probe HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{body}", body.len());
        let response = response_for(&raw, "127.0.0.1");
        assert!(response.starts_with("HTTP/1.1 400 Bad Request\r\n"));
        assert!(response.contains("unknown probe \\\"unknown\\\"; valid probes: ps, gpu, df, mem, uptime"));
    }
}
