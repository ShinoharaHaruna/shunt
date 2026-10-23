//! Dispatcher: drains spool/ into clients over SSH (design.md §4.4).
//! Two-phase, at-least-once end to end: push, ack, delete. Auto-heal
//! restarts a dead client daemon before pushing. An unreachable box
//! (powered off) is normal operation, never an error.

use crate::proto::wire::StatusReport;
use crate::server::registry;
use crate::server::spool;
use crate::server::transport::{queue_exec, Transport};
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};

const DEFAULT_POLL_S: u64 = 15;

/// Dead-box probe backoff (design.md §4.4): once a client's ssh
/// conn_fail streak reaches `CONN_FAIL_ESCALATE` it is probed only
/// every Nth sweep instead of every sweep, so one dead box (~45s of
/// connect retries) stops head-of-line blocking live clients behind
/// it in the entry order. A recovered box clears the streak on its
/// first periodic probe and resumes full service.
const PROBE_BACKOFF_EVERY: u64 = 4;

/// Per-queue reachability for one sweep.
enum Reach {
    /// status --json answered; carries daemon liveness.
    Up { daemon_alive: bool },
    /// This queue's status failed without proof of a machine outage.
    Down,
    /// SSH exhausted its connection retries; siblings share that outage.
    ConnectionDown,
}

fn probe_client(t: &dyn Transport, queue: &registry::ResolvedQueue) -> Reach {
    let client = &queue.client;
    let failures = registry::conn_fail_read(&client.name);
    match queue_exec(t, queue, &["status", "--json"], None) {
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
        Err(e) => {
            eprintln!("dispatch: {} queue at {} status failed: {e:#}", client.name, client.qroot);
            if registry::conn_fail_read(&client.name) > failures {
                Reach::ConnectionDown
            } else {
                Reach::Down
            }
        }
    }
}

/// Push one staged body to the client; returns Ok(name, deduped)
/// on a clean ack. `entry.meta.slug` is already the
/// collision-mangled `<slug>-<id>` (spool::submit); the remote uses
/// it for the pending filename. `deduped` means the remote already
/// held this delivery (`# shunt:id=` match) and returned the
/// existing entry — the normal outcome of a retry after a lost ack.
fn push(t: &dyn Transport, queue: &registry::ResolvedQueue, entry: &crate::proto::task::SpoolEntry, body: &str) -> Result<(String, bool)> {
    let client = &queue.client;
    // `--slug` is the §5 wire spelling; the submit-side slug check
    // guarantees the value never leads with a dash.
    let out = queue_exec(t, queue, &["enqueue", "--json", "--slug", &entry.meta.slug, "--", "-"], Some(body))?;
    // Propagate extract failures (no wire line, protocol mismatch):
    // the entry stays spooled and the error names the cause instead
    // of degrading to a generic "enqueue rejected".
    let v = crate::proto::wire::extract(&out).map_err(|e| anyhow::anyhow!("enqueue on {}: {e:#}", client.name))?;
    if v.get("ok").and_then(|o| o.as_bool()) == Some(true) {
        let name = v.get("name").and_then(|n| n.as_str()).unwrap_or("");
        // A clean ack MUST carry the queue entry name: an empty or
        // malformed one would land in the ledger as "" / garbage and
        // match nothing in any status list forever (`task show`
        // reports `unknown` with no diagnostic, design.md §4.6).
        // Treat it as a failed push — the spool entry stays for retry.
        anyhow::ensure!(queue_name_ok(name), "enqueue on {} acked ok but name {name:?} is not NNNN_<slug>", client.name);
        let deduped = v.get("deduped").and_then(|d| d.as_bool()).unwrap_or(false);
        Ok((name.to_string(), deduped))
    } else {
        let msg = v.get("error").and_then(|e| e.as_str()).unwrap_or("enqueue rejected");
        anyhow::bail!("enqueue on {} failed: {msg}", client.name)
    }
}

/// A real queue entry name is `NNNN_<slug>` (design.md §3.1): four
/// digits, `_`, then a non-empty tail in the `sanitize_slug` charset
/// `[A-Za-z0-9_.-]`. Anything else on a clean ack is a lying remote.
fn queue_name_ok(name: &str) -> bool {
    let Some((seq, tail)) = name.split_once('_') else { return false };
    seq.len() == 4 && seq.bytes().all(|b| b.is_ascii_digit()) && !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
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
    let mut reach: BTreeMap<(String, String), Reach> = BTreeMap::new();
    let mut checked = BTreeSet::new();
    let mut down = BTreeSet::new();
    let mut live = BTreeSet::new();
    let mut pushed = 0;
    let tunnels = match crate::server::tunnel::list() {
        Ok(names) => names,
        Err(e) => {
            eprintln!("dispatch: tunnel discovery failed: {e:#}");
            Vec::new()
        }
    };
    // Keep spool delivery order; recorded tunnels add liveness work even
    // after the spool drains, without probing unrelated registry clients.
    let work = entries.into_iter().map(|entry| (entry.client.clone(), Some(entry))).chain(tunnels.into_iter().map(|name| (name, None)));

    for (name, entry) in work {
        let client = match clients.entry(name.clone()) {
            std::collections::btree_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::btree_map::Entry::Vacant(v) => match registry::load_client(&name) {
                Ok(c) => v.insert(c),
                Err(e) => {
                    eprintln!("dispatch: {name}: {e:#}");
                    continue;
                }
            },
        };
        if entry.is_none() && client.local {
            continue;
        }

        if checked.insert(name.clone()) && registry::conn_fail_read(&name) >= registry::CONN_FAIL_ESCALATE && !idx.is_multiple_of(PROBE_BACKOFF_EVERY) {
            down.insert(name.clone());
        }
        if down.contains(&name) {
            continue;
        }
        // A spool probe already discharged this client's tunnel responsibility.
        if entry.is_none() && live.contains(&name) {
            continue;
        }
        let queue = match &entry {
            Some(entry) => entry.queue.clone(),
            // Tunnel maintenance must not require a configured execution default.
            None => match registry::queue_names(client).into_iter().next() {
                Some(queue) => queue,
                None => continue,
            },
        };
        let target = match registry::resolve_queue(client, Some(&queue)) {
            Ok(target) => target,
            Err(e) => {
                eprintln!("dispatch: {name} queue {queue} retained: {e:#}");
                continue;
            }
        };
        let mut entry = entry;
        if let Some(entry) = &mut entry {
            if let Err(e) = spool::pin_target(entry, &target.client.qroot) {
                eprintln!("dispatch: {} retained: {e:#}", entry.id);
                continue;
            }
        }
        let owned = crate::server::transport::transport_for(client);
        let t: &dyn Transport = forced.unwrap_or_else(|| owned.as_ref());
        let key = (name.clone(), queue.clone());
        if let std::collections::btree_map::Entry::Vacant(v) = reach.entry(key.clone()) {
            let r = probe_client(t, &target);
            if let Reach::Up { daemon_alive } = r {
                if !daemon_alive {
                    let failures = registry::conn_fail_read(&name);
                    match queue_exec(t, &target, &["start", "--json"], None) {
                        Ok(_) => eprintln!("dispatch: {name} queue {queue} daemon restarted"),
                        Err(e) => {
                            eprintln!("dispatch: {name} queue {queue} start failed: {e:#}");
                            if registry::conn_fail_read(&name) > failures {
                                down.insert(name.clone());
                            }
                        }
                    }
                }
                // The machine's tunnel is reconciled once, across all registered roots.
                if !down.contains(&name) && live.insert(name.clone()) && !client.local && crate::server::tunnel::recorded(&name).unwrap_or(false) {
                    let failures = registry::conn_fail_read(&name);
                    match crate::server::tunnel::heal(t, client) {
                        Ok(crate::server::tunnel::Heal::Reconnected(pid)) => eprintln!("dispatch: {name} tunnel reconnected pid {pid}"),
                        Ok(crate::server::tunnel::Heal::Stripped) => eprintln!("dispatch: {name} tunnel dead; ALL_PROXY stripped from env"),
                        Ok(_) => {}
                        Err(e) => eprintln!("dispatch: {name} tunnel heal failed: {e:#}"),
                    }
                    if registry::conn_fail_read(&name) > failures {
                        down.insert(name.clone());
                    }
                }
            }
            if matches!(r, Reach::ConnectionDown) {
                down.insert(name.clone());
            }
            v.insert(r);
        }
        if !matches!(reach[&key], Reach::Up { .. }) || down.contains(&name) {
            continue;
        }
        let Some(entry) = entry else { continue };
        let failures = registry::conn_fail_read(&name);
        let client = &target.client;

        match spool::body(&entry.id).and_then(|b| push(t, &target, &entry, &b)) {
            Ok((name, deduped)) => {
                eprintln!("dispatch: {} -> {} as {name}{}", entry.id, client.name, if deduped { " (deduped)" } else { "" });
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
                // sweep, where box-side `# shunt:id=` dedup returns
                // the existing entry (the task guard is the backstop
                // for remotes that predate the directive).
                if let Err(e) = spool::remove(&entry.id) {
                    eprintln!("dispatch: {} spool remove failed ({e:#}); entry will RE-PUSH next sweep — box-side dedup returns the same name", entry.id);
                }
                pushed += 1;
            }
            Err(e) => eprintln!("dispatch: {} retained: {e:#}", entry.id),
        }
        if registry::conn_fail_read(&client.name) > failures {
            down.insert(client.name.clone());
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
