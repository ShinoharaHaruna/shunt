//! Snapshot assembly for `serve` (design.md §18): the same truth
//! `status --all` computes, split into a slow section (wire probes,
//! one ssh exec per queue) and a fast section (server-local fs
//! reads) so each ticks at its own cost. A failed section reports
//! `{error}` inside the payload — the broadcast itself never dies.

use serde_json::{json, Value};

/// Slow section: parallel wire probes, one thread per registry
/// client — identical data to `status --all`'s clients object,
/// including per-queue `resources` (GPU util/VRAM, RAM, load1).
/// Background mode: the sweep honours the §4.1 probe penalty and the
/// §9 expiry gate, so a dead or expired box is reported from its row
/// instead of burning an ssh timeout every tick.
pub fn clients_section() -> Value {
    match crate::server::status::probe_all(false) {
        Ok(probed) => {
            let mut m = serde_json::Map::new();
            for p in probed {
                m.insert(p.name, json!(p.status));
            }
            Value::Object(m)
        }
        Err(e) => json!({"error": format!("{e:#}")}),
    }
}

/// Fast section: server-local state — spool contents, the task
/// ledger's newest 100 entries (newest first), and the room
/// listings (all rooms; unread computed for `admin`).
pub fn local_section() -> Value {
    let spool = match crate::server::spool::list() {
        Ok(v) => json!({"entries": v}),
        Err(e) => json!({"error": format!("{e:#}")}),
    };
    let tasks = match crate::server::tasks::list() {
        Ok(v) => json!({"entries": v.into_iter().rev().take(100).collect::<Vec<_>>()}),
        Err(e) => json!({"error": format!("{e:#}")}),
    };
    let rooms = match crate::server::room::list(None, true) {
        Ok(l) => json!(l),
        Err(e) => json!({"error": format!("{e:#}")}),
    };
    json!({"spool": spool, "tasks": tasks, "rooms": rooms})
}

/// One room's roster view plus the newest `tail` messages of the
/// merged live+archive stream. `ReadMode::Range` is the audit read:
/// it never advances `admin`'s cursor — the dashboard observes,
/// it does not consume (design.md §17.5's own rule for `status`).
pub fn room_detail(name: &str, tail: usize) -> anyhow::Result<Value> {
    let label = crate::server::room::ADMIN_LABEL;
    let room = crate::server::room::roster(name, Some(label))?;
    let msgs = crate::server::room::read(name, label, crate::server::room::ReadMode::Range { from: None, to: None })?;
    let msgs: Vec<Value> = msgs.into_iter().rev().take(tail).rev().map(|(n, m)| json!({"n": n, "author": m.author, "ts": m.ts, "body_md": m.body_md, "member": m.member, "summary_through": m.summary_through})).collect();
    Ok(json!({"room": room, "msgs": msgs}))
}

/// Task-log tail over the wire (design.md §5): `logs -n <tail>
/// -- <pattern>` resolved through the same `load_queue` ladder as
/// the CLI. A text op — raw stdout passes through, login-shell
/// noise included, exactly what `shunt logs` prints.
pub fn logs(client: &str, queue: Option<&str>, pattern: Option<&str>, tail: usize) -> anyhow::Result<String> {
    use crate::server::{registry, transport};
    let resolved = registry::load_queue(client, queue)?;
    let t = transport::transport_for(&resolved.client);
    let mut argv: Vec<String> = vec!["logs".into(), "-n".into(), tail.to_string()];
    if let Some(p) = pattern {
        argv.push("--".into());
        argv.push(p.to_string());
    }
    let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
    transport::queue_exec(t.as_ref(), &resolved, &refs, None)
}

/// Task script (task.sh) over the wire: `cat -- <name>` resolved
/// through the same `load_queue` ladder as `logs`. A text op — raw
/// stdout passes through verbatim, same as the CLI prints it. This
/// backs the dashboard's pending-entry script pane: a queued entry
/// has no log yet, so the script is what a click has to show.
pub fn script(client: &str, queue: Option<&str>, name: &str) -> anyhow::Result<String> {
    use crate::server::{registry, transport};
    let resolved = registry::load_queue(client, queue)?;
    let t = transport::transport_for(&resolved.client);
    transport::queue_exec(t.as_ref(), &resolved, &["cat", "--", name], None)
}

/// Fixed, read-only system probes exposed by `serve` (§18). The
/// argv is server-owned so request text can never become a command.
pub const PROBE_NAMES: &[&str] = &["ps", "gpu", "df", "mem", "uptime"];

/// Resolve a public probe name to its fixed argv; never pass request
/// text to a shell or append it to these arguments.
pub fn probe_argv(probe: &str) -> Option<&'static [&'static str]> {
    match probe {
        "ps" => Some(&["ps", "-eo", "pid,pgid,etime,%cpu,stat,cmd", "--sort=-%cpu"]),
        "gpu" => Some(&["nvidia-smi"]),
        "df" => Some(&["df", "-h"]),
        "mem" => Some(&["free", "-h"]),
        "uptime" => Some(&["uptime"]),
        _ => None,
    }
}

/// Run a fixed system probe on a registered client (§18).
pub fn probe(client: &str, probe: &str) -> anyhow::Result<String> {
    use crate::server::transport;
    let argv = probe_argv(probe).ok_or_else(|| anyhow::anyhow!("unknown probe {probe:?}"))?;
    let (c, t) = transport::client_transport(client)?;
    let argv: Vec<String> = argv.iter().map(|arg| (*arg).to_string()).collect();
    t.exec(&c, &argv, None)
}

/// Force one status query for a client (`POST /api/reprobe`): bypasses
/// quarantine and expiry, but `client_transport` rejects disabled rows.
/// A successful answer clears the failure streak on the way through.
pub fn reprobe(client: &str) -> anyhow::Result<crate::server::status::ClientStatus> {
    use crate::server::transport;
    let (c, t) = transport::client_transport(client)?;
    Ok(crate::server::status::query(t.as_ref(), &c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probes_resolve_only_to_fixed_argv() {
        assert_eq!(PROBE_NAMES, &["ps", "gpu", "df", "mem", "uptime"]);
        assert_eq!(probe_argv("ps"), Some(&["ps", "-eo", "pid,pgid,etime,%cpu,stat,cmd", "--sort=-%cpu"][..]));
        assert_eq!(probe_argv("gpu"), Some(&["nvidia-smi"][..]));
        assert_eq!(probe_argv("df"), Some(&["df", "-h"][..]));
        assert_eq!(probe_argv("mem"), Some(&["free", "-h"][..]));
        assert_eq!(probe_argv("uptime"), Some(&["uptime"][..]));
        assert_eq!(probe_argv("ps; id"), None);
    }
}
