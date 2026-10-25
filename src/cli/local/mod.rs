//! Local-queue dispatch helpers. These back the local queue
//! subcommands; on a box they double as the SSH wire surface via
//! --json.

use std::path::PathBuf;

use anyhow::Result;

mod control;
mod logs;
mod status;
mod tasks;
#[cfg(test)]
mod tests;

pub(crate) use control::{agent_shutdown, agent_workers, workers_wire_tail};
pub(crate) use logs::{agent_dlog, agent_logs, logs_wire_tail};
pub(crate) use status::agent_status;
pub(crate) use tasks::{agent_cancel, agent_cat, agent_clean, agent_enqueue, agent_kill, agent_new, agent_prio};

pub(crate) fn local_root(flag: &Option<String>) -> Result<PathBuf> {
    crate::client::queue::resolve_root(flag.as_deref())
}

pub(crate) fn jprint(v: impl serde::Serialize) {
    println!("{}", serde_json::to_string(&v).unwrap_or_default());
}

/// json! object -> field map, for building wire responses.
pub(crate) fn jfields(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    v.as_object().cloned().unwrap_or_default()
}

pub(crate) fn jobj(fields: serde_json::Map<String, serde_json::Value>) -> serde_json::Value {
    let mut o = serde_json::Map::new();
    o.insert("protocol".into(), serde_json::json!(crate::proto::wire::PROTOCOL_VERSION));
    o.insert("ok".into(), serde_json::json!(true));
    o.extend(fields);
    serde_json::Value::Object(o)
}

/// Emit the wire error object and exit nonzero, or propagate the
/// error for the human-facing path.
pub(crate) fn jfail(json: bool, msg: &str) -> Result<()> {
    if json {
        jprint(serde_json::json!({"protocol": crate::proto::wire::PROTOCOL_VERSION,
                                  "ok": false, "error": msg}));
        std::process::exit(1);
    }
    anyhow::bail!("{msg}")
}

/// Wrap a `--json`-capable handler body: expected failures already
/// `jfail`, but a stray `?` propagation still owes wire callers the
/// error object (design.md §5) — stderr-only failures surface on the
/// server as "no wire JSON object" instead of the real error.
fn wire(json: bool, r: Result<()>) -> Result<()> {
    match r {
        Ok(()) => Ok(()),
        Err(e) => jfail(json, &format!("{e:#}")),
    }
}
