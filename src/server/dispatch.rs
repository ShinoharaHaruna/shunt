//! Dispatcher: drains spool/ into clients over SSH (design.md §4.5).
//! Two-phase, at-least-once end to end: push, ack, delete. Auto-heal
//! restarts a dead client daemon before pushing. An unreachable box
//! (powered off) is normal operation, never an error.

use crate::proto::wire::StatusReport;
use crate::server::registry;
use crate::server::spool;
use crate::server::transport::{wire_argv, Transport};
use anyhow::Result;
use std::collections::BTreeMap;

const DEFAULT_POLL_S: u64 = 15;

/// Per-client reachability for one sweep.
enum Reach {
    /// status --json answered; carries daemon liveness.
    Up { daemon_alive: bool },
    /// ssh failed entirely (box off). Skip quietly.
    Down,
}

fn probe_client(t: &dyn Transport, client: &registry::ClientEntry) -> Reach {
    match t.exec(client, &wire_argv(client, &["status", "--json"]), None) {
        Ok(out) => match serde_json::from_str::<StatusReport>(out.trim()) {
            Ok(rep) => Reach::Up {
                daemon_alive: rep.daemon.alive,
            },
            Err(_) => Reach::Up {
                daemon_alive: false,
            },
        },
        Err(_) => Reach::Down,
    }
}

/// Push one staged body to the client; returns Ok on a clean ack.
/// `entry.meta.slug` is already the collision-mangled `<slug>-<id>`
/// (spool::submit); the remote uses it for the pending filename.
fn push(
    t: &dyn Transport,
    client: &registry::ClientEntry,
    entry: &crate::proto::task::SpoolEntry,
    body: &str,
) -> Result<String> {
    let out = t.exec(
        client,
        &wire_argv(client, &["enqueue", "-", &entry.meta.slug, "--json"]),
        Some(body),
    )?;
    let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap_or(serde_json::Value::Null);
    if v.get("ok").and_then(|o| o.as_bool()) == Some(true) {
        Ok(v.get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .to_string())
    } else {
        let msg = v
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("enqueue rejected");
        anyhow::bail!("enqueue on {} failed: {msg}", client.name)
    }
}

/// One sweep: push every spool entry whose client is reachable.
/// Transport is resolved per client (`ssh` or the `localhost`
/// loopback). Returns ids successfully delivered.
pub fn sweep() -> Result<usize> {
    sweep_inner(None)
}

/// Same sweep with a forced transport — tests inject a fake here.
pub fn sweep_with(t: &dyn Transport) -> Result<usize> {
    sweep_inner(Some(t))
}

fn sweep_inner(forced: Option<&dyn Transport>) -> Result<usize> {
    let entries = spool::list()?;
    let mut clients: BTreeMap<String, registry::ClientEntry> = BTreeMap::new();
    let mut reach: BTreeMap<String, Reach> = BTreeMap::new();
    let mut pushed = 0;

    for entry in entries {
        let client = match clients.entry(entry.client.clone()) {
            std::collections::btree_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::btree_map::Entry::Vacant(v) => {
                match registry::load_client(&entry.client) {
                    Ok(c) => v.insert(c),
                    Err(e) => {
                        eprintln!("dispatch: {} -> {}: {e:#}", entry.id, entry.client);
                        continue;
                    }
                }
            }
        };

        let owned = crate::server::transport::transport_for(client);
        let t: &dyn Transport = forced.unwrap_or_else(|| owned.as_ref());

        if let std::collections::btree_map::Entry::Vacant(v) = reach.entry(entry.client.clone()) {
            let r = probe_client(t, client);
            if let Reach::Up {
                daemon_alive: false,
            } = r
            {
                // Auto-heal: ssh works but shuntd is dead; start it once.
                match t.exec(client, &wire_argv(client, &["start"]), None) {
                    Ok(_) => eprintln!("dispatch: {} daemon restarted", client.name),
                    Err(e) => eprintln!("dispatch: {} start failed: {e:#}", client.name),
                }
            }
            v.insert(r);
        }
        if matches!(reach[&entry.client], Reach::Down) {
            continue; // box off is normal; leave for next poll
        }

        match spool::body(&entry.id).and_then(|b| push(t, client, &entry, &b)) {
            Ok(name) => {
                eprintln!("dispatch: {} -> {} as {name}", entry.id, client.name);
                spool::remove(&entry.id)?;
                pushed += 1;
            }
            Err(e) => eprintln!("dispatch: {} retained: {e:#}", entry.id),
        }
    }
    Ok(pushed)
}

/// Run the dispatch loop. `once` = single sweep, for tests.
pub fn run(once: bool) -> Result<()> {
    let cfg = registry::load_server_config()?;
    let interval = cfg.poll_interval_s.unwrap_or(DEFAULT_POLL_S);
    loop {
        sweep()?;
        if once {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_secs(interval));
    }
}
