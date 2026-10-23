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

/// Serve identity for `proto::proc::probe`: shunt-family argv[0]
/// basename + argv[1] == `serve` + argv[2] == `run`. Anchoring the
/// leaf keeps a recycled pid from false-identifying (§3.4).
fn is_serve_cmdline(argv: &[&[u8]]) -> bool {
    let Some(argv0) = argv.first() else { return false };
    let base = crate::proto::proc::argv0_base(argv0);
    if !(base == b"shunt" || base == b"shuntd" || base.starts_with(b"shunt-")) {
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

fn ensure_bind_allowed(opts: &ServeOpts) -> Result<()> {
    let loopback = matches!(opts.bind.as_str(), "127.0.0.1" | "127.0.1.1" | "::1" | "localhost");
    anyhow::ensure!(loopback || opts.token.is_some(), "refusing to bind {}:{} without --token — the dashboard exposes fleet state to anyone who can reach the port; bind 127.0.0.1 (and use `ssh -L {0}:{1}:127.0.0.1:{1} <dev>` from your browser) or pass --token", opts.bind, opts.port);
    Ok(())
}

/// `serve up`: detach `serve run …` as a background child — its
/// stdio lands in serve.log, its pid in serve.pid. Idempotent-refuse
/// when one is already alive.
pub fn up(opts: &ServeOpts) -> Result<()> {
    if let Some(pid) = alive()? {
        anyhow::bail!("already running (pid {pid}) — `shunt serve down` first");
    }
    ensure_bind_allowed(opts)?;
    let dir = crate::server::registry::shunt_home()?;
    std::fs::create_dir_all(&dir)?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(log_file()?)?;
    let err = log.try_clone()?;
    let exe = std::env::current_exe()?;
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
    let pid: u32 = std::fs::read_to_string(&pidf).ok().and_then(|s| s.trim().parse().ok()).context("not running — no serve.pid under $SHUNT_HOME")?;
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
            println!("serve down: stopped pid {pid}");
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
    println!("shunt serve: http://{}:{}{token_note} — read-only dashboard; Ctrl-C stops", opts.bind, opts.port);

    // Lifecycle bookkeeping: pidfile first (up/down find us through
    // it), then a signal thread that removes it and exits — TERM
    // and INT both count as a clean stop.
    let pidf = pid_file()?;
    if let Some(dir) = pidf.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    std::fs::write(&pidf, format!("{}", std::process::id()))?;
    {
        let pidf = pidf.clone();
        let mut sigs = signal_hook::iterator::Signals::new([signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT]).context("cannot install signal handlers")?;
        std::thread::spawn(move || {
            let _ = sigs.forever().next();
            let _ = std::fs::remove_file(&pidf);
            std::process::exit(0);
        });
    }

    let auth = Arc::new(opts.token);
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let (latest, hub, auth) = (latest.clone(), hub.clone(), auth.clone());
                std::thread::spawn(move || {
                    let _ = handle(stream, &latest, &hub, &auth);
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

fn authorized(req: &http::Request, token: &Option<String>) -> bool {
    let Some(expected) = token else {
        return true;
    };
    let presented = http::query_param(&req.query, "token").or_else(|| req.header("x-shunt-token").map(str::to_string)).or_else(|| req.header("authorization").and_then(|a| a.strip_prefix("Bearer ").map(str::to_string)));
    presented.as_deref() == Some(expected.as_str())
}

fn handle(mut stream: TcpStream, latest: &Latest, hub: &ws::Hub, token: &Option<String>) -> Result<()> {
    stream.set_nodelay(true).ok();
    let Some(req) = http::read_request(&mut stream) else {
        return Ok(());
    };
    if !authorized(&req, token) {
        return http::json(&mut stream, 401, &json!({"error": "unauthorized — pass ?token=, x-shunt-token, or Authorization: Bearer"}));
    }
    if req.method == "POST" && req.path == "/api/exec" {
        // §18 probe surface: `client exec` passthrough. The only
        // non-GET route — everything else stays read.
        let body: serde_json::Value = match serde_json::from_slice(&req.body) {
            Ok(v) => v,
            Err(_) => return http::json(&mut stream, 400, &json!({"error": "body must be JSON {client, cmd}"})),
        };
        let (client, cmd) = (body["client"].as_str().unwrap_or(""), body["cmd"].as_str().unwrap_or(""));
        if client.is_empty() || cmd.is_empty() {
            return http::json(&mut stream, 400, &json!({"error": "expected {client, cmd}"}));
        }
        return match snapshot::exec(client, cmd) {
            Ok(out) => http::json(&mut stream, 200, &json!({"ok": true, "out": out})),
            Err(e) => http::json(&mut stream, 200, &json!({"ok": false, "error": format!("{e:#}")})),
        };
    }
    if req.method != "GET" {
        return http::json(&mut stream, 405, &json!({"error": "GET only (POST /api/exec is the one probe route)"}));
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
