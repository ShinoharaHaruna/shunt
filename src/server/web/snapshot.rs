//! Snapshot assembly for `serve` (design.md §18): the same truth
//! `status --all` computes, split into a slow section (wire probes,
//! one ssh exec per queue) and a fast section (server-local fs
//! reads) so each ticks at its own cost. A failed section reports
//! `{error}` inside the payload — the broadcast itself never dies.

use serde_json::{json, Value};

/// Slow section: parallel wire probes, one thread per registry
/// client — identical data to `status --all`'s clients object,
/// including per-queue `resources` (GPU util/VRAM, RAM, load1).
pub fn clients_section() -> Value {
    match crate::server::status::probe_all() {
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

/// `client exec` passthrough for read-only probes (`sh -c <cmd>`).
/// The dashboard's one non-GET surface — its scope is the §18
/// probe use, not mutation: nothing here writes queue state.
pub fn exec(client: &str, cmd: &str) -> anyhow::Result<String> {
    use crate::server::transport;
    let (c, t) = transport::client_transport(client)?;
    let argv = vec!["sh".to_string(), "-c".to_string(), cmd.to_string()];
    t.exec(&c, &argv, None)
}
