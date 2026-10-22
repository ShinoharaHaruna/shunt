//! `shunt status --all` / `status -c` (design.md §5, §9): aggregate
//! per-client `status --json` over SSH plus registry billing metadata.
//! Unreachable clients are reported, never fatal.

use crate::server::registry::{self, ClientEntry};
use crate::server::transport::{transport_for, wire_argv, Transport};
use anyhow::Result;
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct ClientStatus {
    pub reachable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub billing: Option<registry::Billing>,
    /// Billing lifetime, verbatim from the row: "unlimited",
    /// "metered", or an ISO-8601 deadline (design.md §4.1, §9).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expire: Option<String>,
    /// Seconds until a timestamped `expire`; omitted for unlimited,
    /// metered, unknown, or a timestamp without an offset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_in_s: Option<i64>,
    /// Consecutive ssh-level connect failures (exit 255). >=
    /// `CONN_FAIL_ESCALATE` means the endpoint likely moved or the
    /// box is down — agents should escalate to the user (§4.1).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connect_failures: Option<u32>,
    /// This vendor reassigns IP/port across restarts (row field);
    /// steers whether "check the console for a new endpoint" applies.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub endpoint_rotates: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub facts: Option<std::collections::BTreeMap<String, serde_json::Value>>,
}

/// Howard Hinnant's days-from-civil: proleptic-Gregorian days since
/// 1970-01-01. std has no datetime math, so we carry the tiny
/// formula ourselves rather than add a dependency for it.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Epoch seconds for an `expire` string that parses as an ISO-8601
/// datetime WITH an offset (`+08:00`/`Z`). Without an offset we
/// cannot know the zone (std has no local-tz) — return None and the
/// row's raw string still reports.
fn expire_epoch(s: &str) -> Option<i64> {
    use toml::value::{Datetime, Offset};
    let dt = s.trim().parse::<Datetime>().ok()?;
    let d = dt.date?;
    let t = dt.time.unwrap_or(toml::value::Time { hour: 0, minute: 0, second: 0, nanosecond: 0 });
    let secs = days_from_civil(i64::from(d.year), i64::from(d.month), i64::from(d.day)) * 86400 + i64::from(t.hour) * 3600 + i64::from(t.minute) * 60 + i64::from(t.second);
    let off = match dt.offset {
        Some(Offset::Z) => 0i64,
        Some(Offset::Custom { minutes }) => i64::from(minutes) * 60,
        None => return None,
    };
    Some(secs - off)
}

fn now_epoch() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Query one client; used by `status -c` and the aggregate loop.
pub fn query(t: &dyn Transport, client: &ClientEntry) -> ClientStatus {
    let expire = (!client.expire.is_empty()).then(|| client.expire.clone());
    let expires_in_s = expire_epoch(&client.expire).map(|e| e - now_epoch());
    let fails = registry::conn_fail_read(&client.name);
    let connect_failures = (fails > 0).then_some(fails);
    let endpoint_rotates = client.endpoint_rotates;
    match t.exec(client, &wire_argv(client, &["status", "--json"]), None) {
        Ok(out) => match crate::proto::wire::extract(&out) {
            Ok(v) => ClientStatus { reachable: true, report: Some(v), error: None, billing: client.billing.clone(), expire, expires_in_s, connect_failures, endpoint_rotates, facts: (!client.facts.is_empty()).then(|| client.facts.clone()) },
            Err(e) => ClientStatus { reachable: true, report: None, error: Some(format!("bad status JSON: {e}")), billing: client.billing.clone(), expire, expires_in_s, connect_failures, endpoint_rotates, facts: None },
        },
        Err(e) => ClientStatus { reachable: false, report: None, error: Some(format!("{e:#}")), billing: client.billing.clone(), expire, expires_in_s, connect_failures, endpoint_rotates, facts: None },
    }
}

/// Aggregate every registered client plus the built-in `localhost`.
/// Transport resolves per client (ssh / loopback). `--json` emits one
/// object; otherwise a table — the server status a human asked for.
/// `as_label` is the §17.5 identity for the `rooms` block: unread
/// counts are computed for it (default $USER); reporting only —
/// status never marks a reader position (polling is awareness).
///
/// Probes run in parallel (std::thread, one per client): a dead box
/// costs its own ssh timeout, not a serial slot ahead of every live
/// client behind it. Output order stays the registry's sorted order
/// regardless of join timing.
pub fn run_all(json: bool, as_label: Option<&str>) -> Result<()> {
    // Resolve once up front: a malformed --as is an error worth
    // surfacing before any probing, not a silently-empty rooms block.
    let room_label = crate::server::room::resolve_label(as_label)?;
    let names = registry::list_clients()?;
    // list_clients keeps rows it cannot parse; a failed load must
    // not kill the whole aggregate — it degrades to an error row,
    // same discipline as an unreachable box.
    let clients: Vec<Result<ClientEntry>> = names.iter().map(|n| registry::load_client(n)).collect();
    // Scoped threads borrow `clients`; each builds its own transport
    // (`Box<dyn Transport>` is not Sync, and transports are cheap).
    let statuses: Vec<ClientStatus> = std::thread::scope(|s| {
        let handles: Vec<Option<_>> = clients
            .iter()
            .map(|c| match c {
                Ok(c) => Some(s.spawn(move || {
                    let t = transport_for(c);
                    query(t.as_ref(), c)
                })),
                Err(_) => None,
            })
            .collect();
        clients
            .iter()
            .zip(handles)
            .map(|(c, h)| match h {
                // A panicked probe degrades to an error row like
                // every other failure mode here — one bad thread
                // must never abort the whole aggregate.
                Some(h) => h.join().unwrap_or_else(|_| ClientStatus { reachable: false, report: None, error: Some("probe panicked".into()), billing: None, expire: None, expires_in_s: None, connect_failures: None, endpoint_rotates: false, facts: None }),
                None => ClientStatus { reachable: false, report: None, error: Some(format!("cannot load row: {:#}", c.as_ref().unwrap_err())), billing: None, expire: None, expires_in_s: None, connect_failures: None, endpoint_rotates: false, facts: None },
            })
            .collect()
    });
    let mut out = serde_json::Map::new();
    let mut rows: Vec<Vec<String>> = Vec::new();
    for ((name, client), st) in names.iter().zip(&clients).zip(statuses) {
        if !json {
            rows.push(match client {
                Ok(client) => row_for(client, &st),
                // Unloadable row: report name + error, no endpoint.
                Err(_) => vec![name.clone(), "?".into(), "-".into(), "-".into(), "-".into(), "-".into(), st.error.as_deref().unwrap_or("unloadable row").lines().next().unwrap_or("unloadable row").chars().take(60).collect()],
            });
        }
        out.insert(name.clone(), serde_json::to_value(&st)?);
    }
    // spool::list() quarantines corrupt entries (`*.bad`, §4.4)
    // even on this read path — accepted: fault isolation wins over
    // read purity, and a status run should surface the same clean
    // spool the dispatcher sees. What it must NOT do is fail the
    // whole command on an unreadable staging dir: degrade to a
    // spool_error field / note line.
    let staged = crate::server::spool::list();
    if json {
        let (pending, err) = match &staged {
            Ok(v) => (serde_json::json!(v.len()), serde_json::Value::Null),
            Err(e) => (serde_json::Value::Null, serde_json::json!(format!("{e:#}"))),
        };
        let rooms = crate::server::room::status_rooms(room_label.as_deref());
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({ "protocol": crate::proto::wire::PROTOCOL_VERSION, "clients": out, "spool_pending": pending, "spool_error": err, "rooms": rooms }))?);
        return Ok(());
    }

    println!("== clients ==  {} registered", names.len());
    let headers = ["name", "transport", "daemon", "pending", "running", "expire", "note"];
    let mut w = headers.map(|h| h.len());
    for r in &rows {
        for (i, c) in r.iter().enumerate() {
            w[i] = w[i].max(c.chars().count());
        }
    }
    let line = |cells: &[String]| {
        print!("  ");
        for (i, c) in cells.iter().enumerate() {
            print!("{c:<width$}  ", width = w[i]);
        }
        println!();
    };
    line(&headers.map(String::from));
    for r in &rows {
        line(r);
    }
    println!();
    match &staged {
        Err(e) => println!("== spool ==  unreadable: {e:#}"),
        Ok(staged) if staged.is_empty() => println!("== spool ==  empty"),
        Ok(staged) => {
            println!("== spool ==  {} staged for dispatch", staged.len());
            for e in staged {
                println!("  {}  {} -> {}  {} ({})", e.id, e.project, e.client, e.meta.slug, e.submitted_at);
            }
        }
    }
    // §17.5: the aggregate carries the coordination inbox too —
    // report-only, and only when rooms exist (zero noise otherwise).
    let rooms = crate::server::room::status_rooms(room_label.as_deref());
    if !rooms.is_empty() {
        println!();
        match &room_label {
            Some(l) => println!("== rooms ==  unread for '{l}'"),
            None => println!("== rooms =="),
        }
        for r in &rooms {
            let unread = r["unread_msgs"].as_u64().map(|n| n.to_string()).unwrap_or_else(|| "-".into());
            let members = r["members"].as_array().map(|a| a.iter().filter_map(|m| m.as_str()).collect::<Vec<_>>().join(",")).unwrap_or_default();
            let dec = r["decision"]["note"].as_str().unwrap_or("-");
            println!("  {}  unread {}  {}  {}", r["name"].as_str().unwrap_or("?"), unread, members, dec);
        }
    }
    Ok(())
}

fn row_for(client: &ClientEntry, st: &ClientStatus) -> Vec<String> {
    let transport = if client.local { "local".to_string() } else { format!("ssh {}@{}:{}", client.ssh.user, client.ssh.host, client.ssh.port) };
    let expire = match expire_epoch(&client.expire) {
        Some(e) => {
            let left = e - now_epoch();
            if left < 0 {
                "EXPIRED".into()
            } else {
                format!("{}h{}m", left / 3600, left % 3600 / 60)
            }
        }
        None => {
            if client.expire.is_empty() {
                "-".into()
            } else {
                client.expire.clone()
            }
        }
    };
    match &st.report {
        Some(rep) => {
            let daemon = if rep.pointer("/daemon/alive").and_then(|v| v.as_bool()).unwrap_or(false) { "up" } else { "down" }.to_string();
            let pending = rep.pointer("/pending").and_then(|v| v.as_array()).map(|a| a.len().to_string()).unwrap_or_else(|| "?".into());
            let running = rep.pointer("/running").and_then(|v| v.as_array()).map(|a| a.len().to_string()).unwrap_or_else(|| "?".into());
            let mut note = client.billing.as_ref().map(|b| format!("{:.2} {}/h", b.hourly, b.currency)).unwrap_or_default();
            if let Some(f) = st.connect_failures {
                note = format!("{note} conn_fail={f}").trim().into();
            }
            vec![client.name.clone(), transport, daemon, pending, running, expire, note]
        }
        None => {
            let err = st.error.as_deref().unwrap_or("unreachable").lines().next().unwrap_or("unreachable");
            let err: String = err.chars().take(60).collect();
            vec![client.name.clone(), transport, "-".into(), "-".into(), "-".into(), expire, err]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expire_parsing() {
        // Keywords and unknowns never produce an epoch.
        assert_eq!(expire_epoch("unlimited"), None);
        assert_eq!(expire_epoch("metered"), None);
        assert_eq!(expire_epoch(""), None);
        assert_eq!(expire_epoch("next friday"), None);
        // Offset-bearing timestamps do; 2026-01-20T18:00+08:00 = 10:00Z.
        let e = expire_epoch("2026-01-20T18:00:00+08:00").unwrap();
        assert_eq!(e, expire_epoch("2026-01-20T10:00:00Z").unwrap());
        // Naive timestamp (no offset): raw reports, epoch skips.
        assert_eq!(expire_epoch("2026-01-20T18:00:00"), None);
        // Date-only parses but carries no offset — same rule.
        assert_eq!(expire_epoch("2026-01-20"), None);
    }
}
