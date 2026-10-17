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
    match t.exec(client, &wire_argv(client, &["status", "--json"]), None) {
        Ok(out) => match serde_json::from_str::<serde_json::Value>(out.trim()) {
            Ok(v) => ClientStatus { reachable: true, report: Some(v), error: None, billing: client.billing.clone(), expire, expires_in_s, facts: (!client.facts.is_empty()).then(|| client.facts.clone()) },
            Err(e) => ClientStatus { reachable: true, report: None, error: Some(format!("bad status JSON: {e}")), billing: client.billing.clone(), expire, expires_in_s, facts: None },
        },
        Err(e) => ClientStatus { reachable: false, report: None, error: Some(format!("{e:#}")), billing: client.billing.clone(), expire, expires_in_s, facts: None },
    }
}

/// Aggregate every registered client plus the built-in `localhost`.
/// Transport resolves per client (ssh / loopback). `--json` emits one
/// object; otherwise a human-readable table.
pub fn run_all(json: bool) -> Result<()> {
    let names = registry::list_clients()?;
    let mut out = serde_json::Map::new();
    for name in &names {
        let client = registry::load_client(name)?;
        let t = transport_for(&client);
        let st = query(t.as_ref(), &client);
        if !json {
            print_human(&client, &st);
        }
        out.insert(name.clone(), serde_json::to_value(&st)?);
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "protocol": crate::proto::wire::PROTOCOL_VERSION,
                "clients": out,
            }))?
        );
    }
    Ok(())
}

fn print_human(client: &ClientEntry, st: &ClientStatus) {
    if let Some(rep) = &st.report {
        let daemon = rep.pointer("/daemon/alive").and_then(|v| v.as_bool()).unwrap_or(false);
        let pending = rep.pointer("/pending").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
        let running = rep.pointer("/running").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
        let rate = client.billing.as_ref().map(|b| format!(" {:.2} {}/h", b.hourly, b.currency)).unwrap_or_default();
        let exp = match expire_epoch(&client.expire) {
            Some(e) => {
                let left = e - now_epoch();
                if left < 0 {
                    " EXPIRED".to_string()
                } else {
                    format!(" expires_in={}h{}m", left / 3600, left % 3600 / 60)
                }
            }
            None if client.expire.is_empty() => String::new(),
            None => format!(" expire={}", client.expire),
        };
        let host = if client.local { "local" } else { client.ssh.host.as_str() };
        println!("{}\t{}\tdaemon={} pending={} running={}{}{}", client.name, host, daemon, pending, running, rate, exp);
    } else {
        println!("{}\tunreachable\t{}", client.name, st.error.as_deref().unwrap_or(""));
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
