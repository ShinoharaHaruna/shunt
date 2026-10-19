//! Dispatcher: drains spool/ into clients over SSH (design.md §4.4).
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

/// Dead-box probe backoff (design.md §4.4): once a client's ssh
/// conn_fail streak reaches `CONN_FAIL_ESCALATE` it is probed only
/// every Nth sweep instead of every sweep, so one dead box (~45s of
/// connect retries) stops head-of-line blocking live clients behind
/// it in the entry order. A recovered box clears the streak on its
/// first periodic probe and resumes full service.
const PROBE_BACKOFF_EVERY: u64 = 4;

/// Per-client reachability for one sweep.
enum Reach {
    /// status --json answered; carries daemon liveness.
    Up { daemon_alive: bool },
    /// ssh failed entirely (box off). Skip quietly.
    Down,
}

fn probe_client(t: &dyn Transport, client: &registry::ClientEntry) -> Reach {
    match t.exec(client, &wire_argv(client, &["status", "--json"]), None) {
        Ok(out) => match crate::proto::wire::parse::<StatusReport>(&out) {
            Ok(rep) => Reach::Up { daemon_alive: rep.daemon.alive },
            // An unparseable reply is protocol drift (binary version
            // skew), not a dead daemon — folding it into Up{false}
            // would `start` the drifted binary and mask the real
            // error. Surface it once and treat as Down this sweep.
            Err(e) => {
                eprintln!("dispatch: {} status reply unparseable (protocol drift?): {e:#}", client.name);
                Reach::Down
            }
        },
        Err(_) => Reach::Down,
    }
}

/// Push one staged body to the client; returns Ok on a clean ack.
/// `entry.meta.slug` is already the collision-mangled `<slug>-<id>`
/// (spool::submit); the remote uses it for the pending filename.
fn push(t: &dyn Transport, client: &registry::ClientEntry, entry: &crate::proto::task::SpoolEntry, body: &str) -> Result<String> {
    // `--` keeps a dash-leading slug positional on the remote; the
    // submit-side slug check should prevent those entirely.
    let out = t.exec(client, &wire_argv(client, &["enqueue", "--json", "--", "-", &entry.meta.slug]), Some(body))?;
    // Propagate extract failures (no wire line, protocol mismatch):
    // the entry stays spooled and the error names the cause instead
    // of degrading to a generic "enqueue rejected".
    let v = crate::proto::wire::extract(&out).map_err(|e| anyhow::anyhow!("enqueue on {}: {e:#}", client.name))?;
    if v.get("ok").and_then(|o| o.as_bool()) == Some(true) {
        Ok(v.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string())
    } else {
        let msg = v.get("error").and_then(|e| e.as_str()).unwrap_or("enqueue rejected");
        anyhow::bail!("enqueue on {} failed: {msg}", client.name)
    }
}

/// One sweep: push every spool entry whose client is reachable.
/// Transport is resolved per client (`ssh` or the `localhost`
/// loopback). Returns ids successfully delivered.
pub fn sweep() -> Result<usize> {
    sweep_inner(None, 0)
}

/// Same sweep with a forced transport — tests inject a fake here.
pub fn sweep_with(t: &dyn Transport) -> Result<usize> {
    sweep_inner(Some(t), 0)
}

/// Forced-transport sweep at an explicit sweep index — tests the
/// probe-backoff path without running the dispatcher loop.
#[doc(hidden)]
pub fn sweep_at(t: &dyn Transport, idx: u64) -> Result<usize> {
    sweep_inner(Some(t), idx)
}

/// `idx` counts sweeps since dispatcher start; it drives dead-box
/// probe backoff (`PROBE_BACKOFF_EVERY`). Sweep 0 (single-shot
/// `dispatch --once`, tests) always probes.
fn sweep_inner(forced: Option<&dyn Transport>, idx: u64) -> Result<usize> {
    let entries = spool::list()?;
    let mut clients: BTreeMap<String, registry::ClientEntry> = BTreeMap::new();
    let mut reach: BTreeMap<String, Reach> = BTreeMap::new();
    let mut pushed = 0;

    for entry in entries {
        let client = match clients.entry(entry.client.clone()) {
            std::collections::btree_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::btree_map::Entry::Vacant(v) => match registry::load_client(&entry.client) {
                Ok(c) => v.insert(c),
                Err(e) => {
                    eprintln!("dispatch: {} -> {}: {e:#}", entry.id, entry.client);
                    continue;
                }
            },
        };

        let owned = crate::server::transport::transport_for(client);
        let t: &dyn Transport = forced.unwrap_or_else(|| owned.as_ref());

        if let std::collections::btree_map::Entry::Vacant(v) = reach.entry(entry.client.clone()) {
            let r = if registry::conn_fail_read(&client.name) >= registry::CONN_FAIL_ESCALATE && !idx.is_multiple_of(PROBE_BACKOFF_EVERY) {
                // Known-dead box between periodic probes: skip the
                // connect entirely this sweep.
                Reach::Down
            } else {
                probe_client(t, client)
            };
            if let Reach::Up { daemon_alive } = r {
                if !daemon_alive {
                    // Auto-heal: ssh works but shuntd is dead; start it once.
                    match t.exec(client, &wire_argv(client, &["start", "--json"]), None) {
                        Ok(_) => eprintln!("dispatch: {} daemon restarted", client.name),
                        Err(e) => eprintln!("dispatch: {} start failed: {e:#}", client.name),
                    }
                }
                // Tunnel self-heal (design.md §4.3): a dead tunnel ssh
                // leaves ALL_PROXY pointing at a dead proxy, poisoning
                // every task on a live box. Only runs when the probe
                // actually happened — backoff-skipped probes above
                // also skip this, so a dead box never pays for it.
                if !client.local && crate::server::tunnel::recorded(&client.name).unwrap_or(false) {
                    match crate::server::tunnel::heal(t, client) {
                        Ok(crate::server::tunnel::Heal::Reconnected(pid)) => eprintln!("dispatch: {} tunnel reconnected pid {pid}", client.name),
                        Ok(crate::server::tunnel::Heal::Stripped) => eprintln!("dispatch: {} tunnel dead; ALL_PROXY stripped from env", client.name),
                        Ok(_) => {}
                        Err(e) => eprintln!("dispatch: {} tunnel heal failed: {e:#}", client.name),
                    }
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
                // Ledger (design.md §4.5): the spool id -> queue
                // name mapping survives only here once the spool
                // entry is gone. Best-effort — a ledger failure
                // must not make a delivered task look undelivered.
                if let Err(e) = crate::server::tasks::record_push(&entry, &name) {
                    eprintln!("dispatch: {} ledger write failed: {e:#}", entry.id);
                }
                // A remove hiccup must not abort the sweep: every
                // remaining deliverable entry would be skipped this
                // poll for one fs error. The entry WAS delivered —
                // log LOUDLY: the leftover spool file re-pushes next
                // sweep (the task guard absorbs the double-run).
                if let Err(e) = spool::remove(&entry.id) {
                    eprintln!("dispatch: {} spool remove failed ({e:#}); entry will RE-PUSH next sweep — the remote guard must absorb the duplicate", entry.id);
                }
                pushed += 1;
            }
            Err(e) => eprintln!("dispatch: {} retained: {e:#}", entry.id),
        }
    }
    Ok(pushed)
}

/// Run the dispatch loop. `once` = single sweep, for tests — errors
/// propagate there. In the looping form a failed sweep (e.g. spool
/// dir unreadable this poll) is logged and retried next cycle; the
/// dispatcher must never exit on a transient fault.
pub fn run(once: bool) -> Result<()> {
    let cfg = registry::load_server_config()?;
    let interval = cfg.poll_interval_s.unwrap_or(DEFAULT_POLL_S);
    let mut idx = 0u64;
    loop {
        if once {
            sweep()?;
            return Ok(());
        }
        if let Err(e) = sweep_inner(None, idx) {
            eprintln!("dispatch: sweep failed, retrying next poll: {e:#}");
        }
        idx = idx.wrapping_add(1);
        std::thread::sleep(std::time::Duration::from_secs(interval));
    }
}
