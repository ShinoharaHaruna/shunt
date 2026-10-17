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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub facts: Option<std::collections::BTreeMap<String, serde_json::Value>>,
}

/// Query one client; used by `status -c` and the aggregate loop.
pub fn query(t: &dyn Transport, client: &ClientEntry) -> ClientStatus {
    match t.exec(client, &wire_argv(client, &["status", "--json"]), None) {
        Ok(out) => match serde_json::from_str::<serde_json::Value>(out.trim()) {
            Ok(v) => ClientStatus { reachable: true, report: Some(v), error: None, billing: client.billing.clone(), facts: (!client.facts.is_empty()).then(|| client.facts.clone()) },
            Err(e) => ClientStatus { reachable: true, report: None, error: Some(format!("bad status JSON: {e}")), billing: client.billing.clone(), facts: None },
        },
        Err(e) => ClientStatus { reachable: false, report: None, error: Some(format!("{e:#}")), billing: client.billing.clone(), facts: None },
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
        let host = if client.local { "local" } else { client.ssh.host.as_str() };
        println!("{}\t{}\tdaemon={} pending={} running={}{}", client.name, host, daemon, pending, running, rate);
    } else {
        println!("{}\tunreachable\t{}", client.name, st.error.as_deref().unwrap_or(""));
    }
}
