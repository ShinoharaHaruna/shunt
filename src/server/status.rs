//! `shunt status --all` / `status -c` (design.md §5, §9): aggregate
//! per-client `status --json` over SSH plus registry billing metadata.
//! Unreachable clients are reported, never fatal.

use crate::server::registry::{self, ClientEntry, InactiveReason};
use crate::server::registry::{expire_epoch, expires_left_s};
use crate::server::remote;
use crate::server::transport::{transport_for, Transport};
use anyhow::Result;
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Serialize)]
pub struct QueueStatus {
    pub qroot: String,
    pub reachable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// §15 binary/libc verdict — `warn:`/`error:` prefixed detail,
    /// present only when not ok (a binary that answered can only be
    /// ok or warn; error is reserved for impossible reported state).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compat: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ClientStatus {
    /// At least one queue answered transport; inspect queues for partial failure.
    pub reachable: bool,
    pub queues: BTreeMap<String, QueueStatus>,
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
    /// Present only when an automatic (background) sweep chose not to
    /// touch this client: the row is reported from the registry alone
    /// and `queues` is empty because nobody asked, not because the box
    /// answered with nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip: Option<ProbeSkip>,
    /// The row is intentionally or automatically inactive, even when an
    /// explicit probe was requested and returned a fresh answer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inactive: Option<InactiveReason>,
}

/// Why a background sweep skipped a client (design.md §4.1, §9, §18).
/// `reason` is a stable token the dashboard maps to a label; both surfaces
/// stay on the same truth.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProbeSkip {
    pub reason: &'static str,
    /// Consecutive connect failures behind a `backoff`/`quarantined` skip.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fails: Option<u32>,
    /// Seconds until the next automatic attempt (`backoff` only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_in_s: Option<i64>,
}

/// Automatic-probe penalty (design.md §4.1). Once a client's ssh
/// streak reaches `CONN_FAIL_ESCALATE` the background sweep retries it
/// on a doubling clock instead of every tick — 30 s, 1 min, 2 min …
/// capped at 10 min — and at `CONN_FAIL_QUARANTINE` consecutive failures
/// it stops entirely until an explicit probe or row update.
const PROBE_PENALTY_BASE_S: i64 = 30;
const PROBE_PENALTY_CAP_S: i64 = 600;
pub const PROBE_GIVE_UP: u32 = registry::CONN_FAIL_QUARANTINE;

fn now_epoch() -> i64 {
    crate::proto::time::unix_now() as i64
}

/// Seconds the background sweep waits after the `streak`-th failure.
fn probe_penalty_s(streak: u32) -> i64 {
    let steps = streak.saturating_sub(registry::CONN_FAIL_ESCALATE);
    PROBE_PENALTY_BASE_S.saturating_mul(1 << steps.min(5)).min(PROBE_PENALTY_CAP_S)
}

/// Decide whether a background probe may touch this client. Inactive
/// rows are always skipped; an explicit probe bypasses expiry/quarantine,
/// but never a disabled row. Remaining failures use the doubling penalty.
fn probe_skip(client: &ClientEntry, streak: u32, last_fail: Option<i64>, now: i64) -> Option<ProbeSkip> {
    match registry::inactive_reason(client, streak) {
        Some(InactiveReason::Disabled) => return Some(ProbeSkip { reason: "disabled", fails: None, retry_in_s: None }),
        Some(InactiveReason::Expired) => return Some(ProbeSkip { reason: "expired", fails: None, retry_in_s: None }),
        Some(InactiveReason::Quarantined { consecutive_failures }) => return Some(ProbeSkip { reason: "quarantined", fails: Some(consecutive_failures), retry_in_s: None }),
        None => {}
    }
    if streak < registry::CONN_FAIL_ESCALATE {
        return None;
    }
    // A streak file without the timestamp (written before this build)
    // has no clock to wait on: probe now, which also stamps it.
    let due = last_fail.map_or(now, |at| at + probe_penalty_s(streak));
    (now < due).then(|| ProbeSkip { reason: "backoff", fails: Some(streak), retry_in_s: Some(due - now) })
}

/// A row a sweep deliberately did not touch: everything the registry
/// itself knows, plus why. `queues` is empty because nobody asked.
fn skipped_status(client: &ClientEntry, skip: ProbeSkip) -> ClientStatus {
    let fails = registry::conn_fail_streak(client);
    ClientStatus { reachable: false, queues: BTreeMap::new(), report: None, error: None, billing: client.billing.clone(), expire: (!client.expire.is_empty()).then(|| client.expire.clone()), expires_in_s: expires_left_s(client), connect_failures: (fails > 0).then_some(fails), endpoint_rotates: client.endpoint_rotates, facts: (!client.facts.is_empty()).then(|| client.facts.clone()), skip: Some(skip), inactive: registry::inactive_reason(client, fails) }
}

/// Query one client; used by `status -c` and the aggregate loop. Explicit
/// probes may bypass expiry/quarantine, but disabled rows never reach transport.
pub fn query(t: &dyn Transport, client: &ClientEntry) -> ClientStatus {
    if client.disabled {
        return skipped_status(client, ProbeSkip { reason: "disabled", fails: None, retry_in_s: None });
    }
    let expire = (!client.expire.is_empty()).then(|| client.expire.clone());
    let expires_in_s = expires_left_s(client);
    let endpoint_rotates = client.endpoint_rotates;
    let mut queues = BTreeMap::new();
    let mut errors = Vec::new();
    match registry::all_queues(client) {
        Ok(targets) => {
            for queue in targets {
                let status = match remote::remote_status(t, &queue) {
                    // `raw`, not a re-serialized report: remote
                    // fields this build doesn't model pass through
                    // to `status -c` verbatim.
                    remote::StatusAnswer::Report { raw, report } => {
                        let compat = compat_of(&report, client);
                        QueueStatus { qroot: queue.client.qroot, reachable: true, report: Some(raw), error: None, compat }
                    }
                    // Drift means the box DID answer — reachable
                    // stays true; only the payload was bad.
                    remote::StatusAnswer::Drift(e) => QueueStatus { qroot: queue.client.qroot, reachable: true, report: None, error: Some(format!("bad status JSON: {e:#}")), compat: None },
                    remote::StatusAnswer::Transport(e) | remote::StatusAnswer::ConnDown(e) => QueueStatus { qroot: queue.client.qroot, reachable: false, report: None, error: Some(format!("{e:#}")), compat: None },
                };
                if let Some(error) = &status.error {
                    errors.push(format!("{}: {error}", queue.name));
                }
                queues.insert(queue.name, status);
            }
        }
        Err(e) => errors.push(format!("cannot resolve queues: {e:#}")),
    }
    let report = if queues.len() == 1 { queues.values().next().and_then(|q| q.report.clone()) } else { None };
    let fails = registry::conn_fail_streak(client);
    let connect_failures = (fails > 0).then_some(fails);
    let inactive = registry::inactive_reason(client, fails);
    ClientStatus { reachable: queues.values().any(|q| q.reachable), queues, report, error: (!errors.is_empty()).then(|| errors.join("; ")), billing: client.billing.clone(), expire, expires_in_s, connect_failures, endpoint_rotates, facts: (!client.facts.is_empty()).then(|| client.facts.clone()), skip: None, inactive }
}

/// §15 three-state verdict for a binary that answered: the wire
/// `needs_glibc` self-report vs the registry `libc` fact. Running
/// ⟹ weak-miss ⟹ warn — a hard miss could never have reported.
/// Ok yields None: silence is the ok state.
fn compat_of(report: &crate::proto::wire::StatusReport, client: &ClientEntry) -> Option<String> {
    use crate::proto::libc;
    use crate::server::client_ops::compat::{reported_verdict, Compat};
    let needs = report.needs_glibc.as_deref().and_then(libc::parse_ver);
    let remote = client.facts.get("libc").and_then(|v| v.as_str()).and_then(libc::parse_libc);
    match reported_verdict(needs, remote.as_ref()) {
        Compat::Ok => None,
        Compat::Warn(w) => Some(format!("warn: {w}")),
        Compat::Error(e) => Some(format!("error: {e}")),
    }
}

/// The compat footnotes for a queried client: one remediation line
/// per queue not in the ok state. `upgrade -c` always ships musl —
/// the ok tier — and relinks even at equal version (§16).
fn compat_footnotes(name: &str, status: &ClientStatus) {
    for (q, s) in &status.queues {
        if let Some(c) = &s.compat {
            eprintln!("note: {name}/{q}: {c} — fix: `shunt upgrade -c {name} --queue {q}`");
        }
    }
}

/// Preserve the single-queue wire surface, but never hide sibling queues
/// behind a configured execution default when an instance status is requested.
pub fn run_client(name: &str, json: bool) -> Result<()> {
    let client = registry::load_client_entry(name)?;
    let transport = transport_for(&client);
    let status = query(transport.as_ref(), &client);
    compat_footnotes(name, &status);
    if status.queues.len() == 1 {
        let queue = status.queues.values().next().unwrap();
        if let Some(report) = &queue.report {
            let mut report = report.clone();
            if let Some(inactive) = &status.inactive {
                report["inactive"] = serde_json::to_value(inactive)?;
            }
            println!("{}", serde_json::to_string_pretty(&report)?);
            return Ok(());
        }
        let reason = status.inactive.as_ref().map(|inactive| format!("; inactive: {inactive}")).unwrap_or_default();
        anyhow::bail!("{}{reason}: {}; re-run `shunt client bootstrap {name}` if the queue binary is missing or stale", name, queue.error.as_deref().unwrap_or("queue status unavailable"));
    }
    if json {
        let mut value = serde_json::to_value(&status)?;
        value["protocol"] = crate::proto::wire::PROTOCOL_VERSION.into();
        value["client"] = name.into();
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!("== {name} ==");
        print_rows(&rows_for(&client, &status));
    }
    Ok(())
}

/// One probed client for the aggregate paths (`status --all`,
/// `serve` snapshots): the registry name, the row load result, and
/// the wire query outcome. A failed row load degrades to an
/// error-carrying status, never an abort of the whole sweep.
pub struct ProbedClient {
    pub name: String,
    pub row: Result<ClientEntry>,
    pub status: ClientStatus,
}

/// Probe every registered client in parallel (std::thread, one per
/// client): a dead box costs its own ssh timeout, not a serial slot
/// ahead of every live client behind it. Order stays the registry's
/// sorted order regardless of join timing.
///
/// `force` picks the policy: `true` is an explicit request (`status
/// --all`, the dashboard's "probe now") and bypasses expiry/quarantine;
/// disabled rows remain non-probeable. `false` is the background sweep
/// and honours `probe_skip`, reporting inactive/backoff rows from the registry.
pub fn probe_all(force: bool) -> Result<Vec<ProbedClient>> {
    let names = registry::list_client_names()?;
    let clients: Vec<Result<ClientEntry>> = names.iter().map(|n| registry::load_client_entry(n)).collect();
    // Skipped rows never reach `spawn`: the saving IS the point.
    let now = now_epoch();
    let skip_for = |c: &ClientEntry| probe_skip(c, registry::conn_fail_streak(c), registry::conn_fail_at(&c.name), now);
    let skips: Vec<Option<ProbeSkip>> = clients.iter().map(|c| c.as_ref().ok().and_then(|c| if force && !c.disabled { None } else { skip_for(c) })).collect();
    // Scoped threads borrow `clients`; each builds its own transport
    // (`Box<dyn Transport>` is not Sync, and transports are cheap).
    let statuses: Vec<ClientStatus> = std::thread::scope(|s| {
        let handles: Vec<Option<_>> = clients
            .iter()
            .zip(&skips)
            .map(|(c, skip)| match (c, skip) {
                (Ok(c), None) => Some(s.spawn(move || {
                    let t = transport_for(c);
                    query(t.as_ref(), c)
                })),
                _ => None,
            })
            .collect();
        clients
            .iter()
            .zip(&skips)
            .zip(handles)
            .map(|((c, skip), h)| match (skip, h) {
                (Some(skip), _) => skipped_status(c.as_ref().expect("a skip needs a loaded row"), skip.clone()),
                // A panicked probe degrades to an error row like
                // every other failure mode here — one bad thread
                // must never abort the whole aggregate.
                (None, Some(h)) => h.join().unwrap_or_else(|_| ClientStatus { reachable: false, queues: BTreeMap::new(), report: None, error: Some("probe panicked".into()), billing: None, expire: None, expires_in_s: None, connect_failures: None, endpoint_rotates: false, facts: None, skip: None, inactive: None }),
                (None, None) => ClientStatus { reachable: false, queues: BTreeMap::new(), report: None, error: Some(format!("cannot load row: {:#}", c.as_ref().unwrap_err())), billing: None, expire: None, expires_in_s: None, connect_failures: None, endpoint_rotates: false, facts: None, skip: None, inactive: None },
            })
            .collect()
    });
    Ok(names.into_iter().zip(clients).zip(statuses).map(|((name, row), status)| ProbedClient { name, row, status }).collect())
}

/// Aggregate every registered client plus the built-in `localhost`.
/// Transport resolves per client (ssh / loopback). `--json` emits one
/// object; otherwise a table — the server status a human asked for.
/// `as_label` is the §17.5 identity for the `rooms` block: unread
/// counts are computed for it (default `admin`); reporting only —
/// status never marks a reader position (polling is awareness).
pub fn run_all(json: bool, as_label: Option<&str>) -> Result<()> {
    // Resolve once up front: a malformed --as is an error worth
    // surfacing before any probing, not a silently-empty rooms block.
    let room_label = crate::server::room::resolve_label(as_label)?;
    // An explicit `status --all` is a request, not a poll: it probes
    // every row even when the background sweep has given up on it.
    let probed = probe_all(true)?;
    let mut out = serde_json::Map::new();
    let mut rows: Vec<Vec<String>> = Vec::new();
    for p in &probed {
        let (name, st) = (&p.name, &p.status);
        if !json {
            compat_footnotes(name, st);
            match &p.row {
                Ok(client) => rows.extend(rows_for(client, st)),
                Err(_) => rows.push(vec![name.clone(), "?".into(), "-".into(), "-".into(), "-".into(), "-".into(), st.error.as_deref().unwrap_or("unloadable row").lines().next().unwrap_or("unloadable row").chars().take(60).collect()]),
            }
        }
        out.insert(name.clone(), serde_json::to_value(st)?);
    }
    let names: Vec<String> = probed.iter().map(|p| p.name.clone()).collect();
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
        let rooms = crate::server::room::status_rooms(Some(&room_label));
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({ "protocol": crate::proto::wire::PROTOCOL_VERSION, "clients": out, "spool_pending": pending, "spool_error": err, "rooms": rooms }))?);
        return Ok(());
    }

    println!("== clients ==  {} registered", names.len());
    print_rows(&rows);
    println!();
    match &staged {
        Err(e) => println!("== spool ==  unreadable: {e:#}"),
        Ok(staged) if staged.is_empty() => println!("== spool ==  empty"),
        Ok(staged) => {
            println!("== spool ==  {} staged for dispatch", staged.len());
            for e in staged {
                println!("  {}  {} -> {}/{}  {} ({})", e.id, e.project, e.client, e.queue, e.meta.slug, e.submitted_at);
            }
        }
    }
    // §17.5: the aggregate carries the coordination inbox too —
    // report-only, and only when rooms exist (zero noise otherwise).
    let rooms = crate::server::room::status_rooms(Some(&room_label));
    if !rooms.is_empty() {
        println!();
        println!("== rooms ==  unread for '{room_label}'");
        for r in &rooms {
            let unread = r["unread_msgs"].as_u64().map(|n| n.to_string()).unwrap_or_else(|| "-".into());
            let members = r["members"].as_array().map(|a| a.iter().filter_map(|m| m.as_str()).collect::<Vec<_>>().join(",")).unwrap_or_default();
            let dec = r["decision"]["note"].as_str().unwrap_or("-");
            println!("  {}  unread {}  {}  {}", r["name"].as_str().unwrap_or("?"), unread, members, dec);
        }
    }
    Ok(())
}

fn print_rows(rows: &[Vec<String>]) {
    let headers = ["name/queue", "transport", "daemon", "pending", "running", "expire", "note"];
    let mut widths = headers.map(str::len);
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let line = |cells: &[String]| {
        print!("  ");
        for (i, cell) in cells.iter().enumerate() {
            print!("{cell:<width$}  ", width = widths[i]);
        }
        println!();
    };
    line(&headers.map(String::from));
    for row in rows {
        line(row);
    }
}

fn rows_for(client: &ClientEntry, st: &ClientStatus) -> Vec<Vec<String>> {
    if st.queues.is_empty() {
        return vec![row_for(client, st, None)];
    }
    st.queues
        .iter()
        .enumerate()
        .map(|(index, (name, queue))| {
            let mut row = row_for(client, st, Some(queue));
            row[0] = format!("{}/{name}", client.name);
            // Instance billing and expiry belong to one row, not every queue.
            if index > 0 {
                row[1].clear();
                row[5].clear();
                if queue.error.is_none() {
                    row[6].clear();
                }
            }
            row
        })
        .collect()
}

/// Human note for a row the background sweep deliberately skipped
/// (design.md §4.1 penalty, §9 expiry).
fn skip_note(skip: &ProbeSkip) -> String {
    match skip.reason {
        "disabled" => "disabled — automatic probing off".into(),
        "expired" => "expired — automatic probing off".into(),
        "quarantined" | "gave_up" => format!("quarantined ({} consecutive failures)", skip.fails.unwrap_or_default()),
        "backoff" => format!("paused: {} failed probes, next try in {}m", skip.fails.unwrap_or_default(), (skip.retry_in_s.unwrap_or(0) + 59) / 60),
        other => format!("skipped ({other})"),
    }
}

fn row_for(client: &ClientEntry, st: &ClientStatus, queue: Option<&QueueStatus>) -> Vec<String> {
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
    let report = queue.and_then(|q| q.report.as_ref()).or(st.report.as_ref());
    let error = queue.and_then(|q| q.error.as_deref()).or(st.error.as_deref());
    match report {
        Some(rep) => {
            let daemon = if rep.pointer("/daemon/alive").and_then(|v| v.as_bool()).unwrap_or(false) { "up" } else { "down" }.to_string();
            let pending = rep.pointer("/pending").and_then(|v| v.as_array()).map(|a| a.len().to_string()).unwrap_or_else(|| "?".into());
            let running = rep.pointer("/running").and_then(|v| v.as_array()).map(|a| a.len().to_string()).unwrap_or_else(|| "?".into());
            let mut note = client.billing.as_ref().map(|b| format!("{:.2} {}/h", b.hourly, b.currency)).unwrap_or_default();
            if let Some(f) = st.connect_failures {
                note = format!("{note} conn_fail={f}").trim().into();
            }
            if let Some(c) = queue.and_then(|q| q.compat.as_deref()) {
                // The detail prints in the footnote; the cell carries
                // the state tag only.
                note = format!("{note} bin:{}", c.split(':').next().unwrap_or(c)).trim().into();
            }
            if let Some(inactive) = &st.inactive {
                note = format!("{note} inactive: {inactive}").trim().into();
            }
            vec![client.name.clone(), transport, daemon, pending, running, expire, note]
        }
        None => {
            let inactive = st.inactive.as_ref().map(ToString::to_string);
            let skip = st.skip.as_ref().map(skip_note);
            let err = error.map(|err| err.lines().next().unwrap_or("unreachable").chars().take(60).collect::<String>());
            let note = match (inactive.or(skip), err) {
                (Some(reason), Some(error)) => format!("{reason}: {error}"),
                (Some(reason), None) => reason,
                (None, Some(error)) => error,
                (None, None) => "unreachable".into(),
            };
            vec![client.name.clone(), transport, "-".into(), "-".into(), "-".into(), expire, note]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(expire: &str) -> ClientEntry {
        ClientEntry { name: "box".into(), local: false, disabled: false, local_bin: None, bin_path: None, timeout_s: None, ssh_timeout_s: None, pre_connect: String::new(), shutdown_cmd: None, expire: expire.into(), env_setup: String::new(), shell: String::new(), endpoint_rotates: false, shared: false, exit_on_idle: false, ssh: registry::SshEndpoint::default(), qroot: "/q".into(), deploy_root: "/root".into(), facts: Default::default(), billing: None, queues: Default::default(), default_queue: None }
    }

    /// The penalty doubles from 30 s per failure past the escalation
    /// streak and stops growing at 10 min.
    #[test]
    fn penalty_doubles_then_caps() {
        assert_eq!(probe_penalty_s(registry::CONN_FAIL_ESCALATE), 30);
        assert_eq!(probe_penalty_s(4), 60);
        assert_eq!(probe_penalty_s(5), 120);
        assert_eq!(probe_penalty_s(8), 600);
        assert_eq!(probe_penalty_s(99), 600);
    }

    #[test]
    fn skip_gates_expiry_cap_and_penalty_window() {
        let now = 1_000_000;
        let live = row("2999-01-01T00:00:00Z");
        // A healthy streak is probed every tick, no skip at all.
        assert_eq!(probe_skip(&live, 0, None, now), None);
        assert_eq!(probe_skip(&live, 2, Some(now), now), None);
        // Escalated: inside the window it backs off, past it, it is due.
        assert_eq!(probe_skip(&live, 3, Some(now), now), Some(ProbeSkip { reason: "backoff", fails: Some(3), retry_in_s: Some(30) }));
        assert_eq!(probe_skip(&live, 3, Some(now - 31), now), None);
        // No timestamp (pre-upgrade streak file) never blocks a probe.
        assert_eq!(probe_skip(&live, 3, None, now), None);
        // The cap stops the background sweep outright.
        assert_eq!(probe_skip(&live, PROBE_GIVE_UP, Some(now), now), Some(ProbeSkip { reason: "quarantined", fails: Some(PROBE_GIVE_UP), retry_in_s: None }));
        // Expiry outranks every streak state — infinite failures never
        // earn another background attempt.
        assert_eq!(probe_skip(&row("2020-01-01T00:00:00Z"), 0, None, now), Some(ProbeSkip { reason: "expired", fails: None, retry_in_s: None }));
        assert_eq!(probe_skip(&row("2020-01-01T00:00:00Z"), PROBE_GIVE_UP, Some(now), now), Some(ProbeSkip { reason: "expired", fails: None, retry_in_s: None }));
    }

    /// A skipped row still carries what the registry knows, and its
    /// table note says why nobody asked the box — never a bare
    /// "unreachable", which would read as a failed probe.
    #[test]
    fn skipped_status_reports_reason_and_registry_facts() {
        let client = row("2020-01-01T00:00:00Z");
        let st = skipped_status(&client, ProbeSkip { reason: "expired", fails: None, retry_in_s: None });
        assert!(!st.reachable);
        assert!(st.queues.is_empty());
        assert_eq!(st.skip.as_ref().map(|s| s.reason), Some("expired"));
        assert!(st.expires_in_s.is_some_and(|left| left < 0));
        let note = rows_for(&client, &st)[0][6].clone();
        assert_eq!(note, "expired");
        let quarantined = skip_note(&ProbeSkip { reason: "quarantined", fails: Some(PROBE_GIVE_UP), retry_in_s: None });
        assert_eq!(quarantined, "quarantined (9 consecutive failures)");
        let backoff = skip_note(&ProbeSkip { reason: "backoff", fails: Some(4), retry_in_s: Some(61) });
        assert!(backoff.contains("2m"), "{backoff}");
    }
}
