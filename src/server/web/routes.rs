//! Route dispatch for `serve` (design.md §18): seven read-only
//! GETs, the two POSTs that never carry a command string, then the
//! embedded-SPA fallback. `handle` runs the `guard` gates first —
//! Host, origin, token — so no route can be reached around them.

use super::guard;
use super::http;
use super::snapshot;
use super::ws;
use super::{assets, publish, Latest, ROOM_MSG_TAIL};
use anyhow::Result;
use serde_json::json;
use std::net::TcpStream;

#[derive(serde::Deserialize)]
pub(super) struct ProbeRequest {
    pub(super) client: String,
    pub(super) probe: String,
}

#[derive(serde::Deserialize)]
struct ReprobeRequest {
    client: String,
}

pub(super) fn handle(mut stream: TcpStream, latest: &Latest, hub: &ws::Hub, token: &Option<String>, bind: &str) -> Result<()> {
    stream.set_nodelay(true).ok();
    let Some(req) = http::read_request(&mut stream) else {
        return Ok(());
    };
    if guard::is_loopback_bind(bind) && !guard::loopback_host_allowed(req.header("host"), bind) {
        return http::json(&mut stream, 403, &json!({"error": "invalid Host for loopback bind"}));
    }
    if !guard::same_origin_ok(&req) {
        return http::json(&mut stream, 403, &json!({"error": "cross-origin request rejected"}));
    }
    if !guard::authorized(&req, token) {
        return http::json(&mut stream, 401, &json!({"error": "unauthorized — pass ?token=, x-shunt-token, or Authorization: Bearer"}));
    }
    if req.method == "POST" {
        return match req.path.as_str() {
            "/api/probe" => probe_post(&mut stream, &req),
            "/api/reprobe" => reprobe_post(&mut stream, &req, latest, hub),
            _ => http::json(&mut stream, 405, &json!({"error": "GET only (POST /api/probe is the one probe route)"})),
        };
    }
    if req.method != "GET" {
        return http::json(&mut stream, 405, &json!({"error": "GET only (POST /api/probe is the one probe route)"}));
    }
    match req.path.as_str() {
        "/ws" => ws::upgrade(stream, &req, hub.clone(), latest),
        "/api/snapshot" => http::json(&mut stream, 200, &latest.read().unwrap()),
        p if p.starts_with("/api/room/") => room_route(&mut stream, p.trim_start_matches("/api/room/")),
        p if p.starts_with("/api/logs/") => logs_route(&mut stream, p, &req.query),
        p if p.starts_with("/api/script/") => script_route(&mut stream, p, &req.query),
        p => spa_route(&mut stream, p),
    }
}

/// §18 exposes fixed inspection commands only; extra JSON fields are
/// ignored and cannot alter the selected argv.
fn probe_post(stream: &mut TcpStream, req: &http::Request) -> Result<()> {
    let body: ProbeRequest = match serde_json::from_slice(&req.body) {
        Ok(v) => v,
        Err(_) => return http::json(stream, 400, &json!({"error": "body must be JSON {client, probe}"})),
    };
    if body.client.is_empty() || body.probe.is_empty() {
        return http::json(stream, 400, &json!({"error": "expected {client, probe}"}));
    }
    if snapshot::probe_argv(&body.probe).is_none() {
        return http::json(stream, 400, &json!({"error": format!("unknown probe {:?}; valid probes: {}", body.probe, snapshot::PROBE_NAMES.join(", "))}));
    }
    match snapshot::probe(&body.client, &body.probe) {
        Ok(out) => http::json(stream, 200, &json!({"ok": true, "out": out})),
        Err(e) => http::json(stream, 200, &json!({"ok": false, "error": format!("{e:#}")})),
    }
}

/// The deliberate "probe now" (§4.1, §18): an explicit status query
/// for one client, bypassing the background penalty and the expiry
/// gate — this call IS the decision to spend an ssh. The answer
/// merges into the live snapshot so every dashboard keeps reading
/// one truth (the next sweep recomputes it).
fn reprobe_post(stream: &mut TcpStream, req: &http::Request, latest: &Latest, hub: &ws::Hub) -> Result<()> {
    let body: ReprobeRequest = match serde_json::from_slice(&req.body) {
        Ok(v) => v,
        Err(_) => return http::json(stream, 400, &json!({"error": "body must be JSON {client}"})),
    };
    if body.client.is_empty() {
        return http::json(stream, 400, &json!({"error": "expected {client}"}));
    }
    match snapshot::reprobe(&body.client) {
        Ok(st) => {
            let name = body.client.clone();
            publish(latest, hub, |s| {
                if let Some(clients) = s["clients"].as_object_mut() {
                    clients.insert(name.clone(), json!(st));
                }
            });
            http::json(stream, 200, &json!({"ok": true, "status": st}))
        }
        Err(e) => http::json(stream, 200, &json!({"ok": false, "error": format!("{e:#}")})),
    }
}

fn room_route(stream: &mut TcpStream, name: &str) -> Result<()> {
    match snapshot::room_detail(name, ROOM_MSG_TAIL) {
        Ok(v) => http::json(stream, 200, &v),
        Err(e) => http::json(stream, 404, &json!({"error": format!("{e:#}")})),
    }
}

/// `/api/logs/<client>/<queue>?pattern=<pat>&tail=N` — queue may be
/// empty (single-queue client).
fn logs_route(stream: &mut TcpStream, path: &str, query: &str) -> Result<()> {
    let segs: Vec<&str> = path.trim_start_matches("/api/logs/").split('/').collect();
    let client = segs.first().copied().unwrap_or("");
    let queue = segs.get(1).copied().filter(|q| !q.is_empty());
    let pattern = http::query_param(query, "pattern");
    let tail = http::query_param(query, "tail").and_then(|t| t.parse::<usize>().ok()).unwrap_or(200).min(2000);
    match snapshot::logs(client, queue, pattern.as_deref(), tail) {
        Ok(text) => http::json(stream, 200, &json!({"ok": true, "text": text})),
        Err(e) => http::json(stream, 200, &json!({"ok": false, "error": format!("{e:#}")})),
    }
}

/// `/api/script/<client>/<queue>?name=<entry>` — the entry's
/// task.sh verbatim; queue may be empty (single-queue client). Same
/// text-op shape as `/api/logs`.
fn script_route(stream: &mut TcpStream, path: &str, query: &str) -> Result<()> {
    let segs: Vec<&str> = path.trim_start_matches("/api/script/").split('/').collect();
    let client = segs.first().copied().unwrap_or("");
    let queue = segs.get(1).copied().filter(|q| !q.is_empty());
    let name = http::query_param(query, "name").unwrap_or_default();
    match snapshot::script(client, queue, &name) {
        Ok(text) => http::json(stream, 200, &json!({"ok": true, "text": text})),
        Err(e) => http::json(stream, 200, &json!({"ok": false, "error": format!("{e:#}")})),
    }
}

/// Everything else is a static asset from the embedded `web/dist`
/// table; a miss returns 404 rather than the SPA shell.
fn spa_route(stream: &mut TcpStream, path: &str) -> Result<()> {
    match assets::lookup(path) {
        Some((bytes, ctype)) => {
            // Vite emits hashed filenames under assets/ — safe to
            // cache forever; index.html must always revalidate.
            let cache = if path.starts_with("/assets/") { "public, max-age=31536000, immutable" } else { "no-cache" };
            // script-src carries the sha256 of index.html's
            // inline theme-init script (the only inline script
            // shipped) — edit that script, recompute its sha256.
            http::respond(stream, 200, "OK", ctype, bytes, &[("cache-control", cache), ("content-security-policy", "default-src 'self'; script-src 'self' 'sha256-jX/bClGWvhh+f5nuFSHuWzYPMxe/COBXGImDlm5JWxc='")])
        }
        None => http::json(stream, 404, &json!({"error": format!("not found: {path}")})),
    }
}
