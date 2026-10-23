//! Task ledger (design.md §4.5). When the dispatcher's push is acked
//! it records which queue name the spool id became on the box —
//! `tasks/<id>.json`, one file per task — so a submission stays
//! traceable after its spool entry is deleted. The box never reports
//! back, so terminal outcomes are resolved lazily: `task show`
//! re-queries the client's `status` and writes a fresh `done`/`failed`
//! observation back into the record (and one line into
//! `events.jsonl`) the first time it is seen. This is the minimal
//! spine of task tracking — a ledger an agent can grep — not a
//! notification system.

use crate::proto::task::SpoolEntry;
use crate::server::registry::{self, ClientEntry};
use crate::server::remote;
use crate::server::transport::Transport;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// One ledger entry: what a spool id became on the box, and — once
/// observed — how it ended.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    /// Server-side task id (the former spool id).
    pub id: String,
    pub project: String,
    pub client: String,
    #[serde(default = "crate::proto::task::default_queue_name")]
    pub queue: String,
    #[serde(default)]
    pub qroot: Option<String>,
    /// Slug as pushed (`<slug>-<shortid>`).
    pub slug: String,
    /// Queue entry name the box acked (`NNNN_<slug>`) — the handle
    /// for `logs`/`kill`/`cancel` against the client.
    pub queue_name: String,
    pub submitted_at: String,
    pub pushed_at: String,
    /// Terminal outcome once lazily observed; absent while the task
    /// is pending/running/unknown (or never checked).
    #[serde(default)]
    pub outcome: Option<TaskOutcome>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskOutcome {
    /// "done" | "failed" — matches the track the entry landed in.
    pub state: String,
    pub rc: i32,
    /// When the server first observed the finish, not when it
    /// happened (the box's own log carries the true wall-clock end).
    pub observed_at: String,
}

/// What a live status query says about the recorded queue name.
#[derive(Debug)]
pub enum Observed {
    Pending,
    Running,
    Done(i32),
    Failed(i32),
    /// In no reported list: gone from the recent window, cancelled,
    /// or renamed — the honest answer is "don't know".
    Unknown,
    /// Transport or wire failure; the error string is the reason.
    Unreachable(String),
}

fn tasks_dir() -> Result<PathBuf> {
    Ok(registry::shunt_home()?.join("tasks"))
}

fn events_path() -> Result<PathBuf> {
    Ok(registry::shunt_home()?.join("events.jsonl"))
}

fn now_iso() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    crate::server::spool::iso_utc(secs)
}

/// Append one event line to `events.jsonl`. A single `write_all`
/// under O_APPEND is atomic for lines well under PIPE_BUF, which is
/// all the two writers (dispatcher push, `task show` observe) ever
/// produce — so the JSON and its `\n` go out in ONE write, never two.
fn append_event(fields: serde_json::Value) -> Result<()> {
    let path = events_path()?;
    if let Some(d) = path.parent() {
        crate::server::fsutil::private_dir(d)?;
    }
    let mut line = serde_json::json!({ "ts": now_iso() });
    line.as_object_mut().unwrap().extend(fields.as_object().cloned().unwrap_or_default());
    let mut buf = serde_json::to_string(&line)?;
    buf.push('\n');
    let mut f = fs::OpenOptions::new().append(true).create(true).mode(0o600).open(&path)?;
    f.write_all(buf.as_bytes())?;
    // Re-tighten a pre-existing loose file (mode above only applies
    // at creation).
    fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    Ok(())
}

fn save(rec: &TaskRecord) -> Result<()> {
    // The id is joined into the record path and the tmp name
    // below — refuse a bad id at the sink so NO caller (record_push,
    // observe write-back, retarget_client) can escape tasks/.
    crate::server::spool::valid_task_id(&rec.id)?;
    let dir = tasks_dir()?;
    crate::server::fsutil::private_dir(&dir)?;
    let path = dir.join(format!("{}.json", rec.id));
    // pid in the tmp name (queue.rs enqueue convention): concurrent
    // savers — `task show` lazy outcome write-back racing
    // `client rename`'s retarget_client — share an id's tmp path
    // otherwise, and the loser's torn write renames over the winner.
    let tmp = dir.join(format!(".{}.{}.json.tmp", rec.id, std::process::id()));
    crate::server::fsutil::write_private(&tmp, serde_json::to_string_pretty(rec)?)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// Record a successful push: spool id -> queue name on the box.
/// Called by the dispatcher after the clean ack, before the spool
/// entry is deleted — the ledger is the only place the mapping
/// survives.
pub fn record_push(entry: &SpoolEntry, queue_name: &str) -> Result<()> {
    let rec = TaskRecord { id: entry.id.clone(), project: entry.project.clone(), client: entry.client.clone(), queue: entry.queue.clone(), qroot: entry.qroot.clone(), slug: entry.meta.slug.clone(), queue_name: queue_name.to_string(), submitted_at: entry.submitted_at.clone(), pushed_at: now_iso(), outcome: None };
    save(&rec)?;
    append_event(serde_json::json!({
        "event": "pushed", "id": rec.id, "client": rec.client,
        "queue": rec.queue_name, "target_queue": rec.queue, "qroot": rec.qroot,
    }))?;
    Ok(())
}

/// Load one ledger record by task id.
pub fn load(id: &str) -> Result<TaskRecord> {
    // The id is joined into a filename — same traversal rule as the
    // spool (`task show ../../x` must not read outside tasks/).
    crate::server::spool::valid_task_id(id)?;
    let path = tasks_dir()?.join(format!("{id}.json"));
    let text = fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
    let rec: TaskRecord = serde_json::from_str(&text).with_context(|| format!("bad JSON in {}", path.display()))?;
    // A planted record whose inner id differs from its filename
    // would re-save (observe write-back, retarget_client) under the
    // inner id — same rule spool::read_entry enforces.
    anyhow::ensure!(rec.id == id, "{}: id {:?} does not match its filename", path.display(), rec.id);
    Ok(rec)
}

/// All ledger records, pushed_at order (id breaks ties). Unparseable
/// files warn to stderr and skip — same per-entry fault isolation as
/// the spool.
pub fn list() -> Result<Vec<TaskRecord>> {
    let dir = tasks_dir()?;
    let mut out = Vec::new();
    match fs::read_dir(&dir) {
        Ok(rd) => {
            for ent in rd.flatten() {
                let name = ent.file_name().to_string_lossy().into_owned();
                if !name.ends_with(".json") || name.starts_with('.') {
                    continue;
                }
                match fs::read_to_string(ent.path()).ok().and_then(|t| serde_json::from_str::<TaskRecord>(&t).ok()) {
                    // Same safety rule as spool::read_entry: the
                    // parsed id must be a safe filename equal to the
                    // entry's own stem, or a planted
                    // `"id":"../../y"` would be re-joined by
                    // save()/retarget_client and write outside
                    // tasks/. Offenders skip with a warning like
                    // unparseable files.
                    Some(r) => {
                        let stem = name.strip_suffix(".json").unwrap_or(&name);
                        if crate::server::spool::valid_task_id(&r.id).is_ok() && r.id == stem {
                            out.push(r);
                        } else {
                            eprintln!("tasks: {} has unsafe or mismatched id {:?}; skipping", ent.path().display(), r.id);
                        }
                    }
                    None => eprintln!("tasks: cannot parse {}; skipping", ent.path().display()),
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", dir.display())),
    }
    out.sort_by(|a, b| a.pushed_at.cmp(&b.pushed_at).then(a.id.cmp(&b.id)));
    Ok(out)
}

/// Lazily resolve the record's outcome against the client's live
/// `status`. Terminal states (done/failed) are written back to the
/// ledger and appended to `events.jsonl` exactly once; transient
/// states (pending/running/unknown) are reported but not recorded —
/// the next `task show` asks again. An already-recorded outcome is
/// returned without any wire call.
pub fn observe(t: &dyn Transport, client: &ClientEntry, rec: &mut TaskRecord) -> Result<Observed> {
    if let Some(o) = &rec.outcome {
        return Ok(if o.state == "done" { Observed::Done(o.rc) } else { Observed::Failed(o.rc) });
    }
    let target = match (|| -> Result<_> {
        anyhow::ensure!(client.name == rec.client, "task {} targets client {}, not {}; load its recorded client before observing", rec.id, rec.client, client.name);
        let target = registry::resolve_queue(client, Some(&rec.queue))?;
        crate::server::spool::validate_qroot(rec.qroot.as_deref(), &target.client.qroot)?;
        Ok(target)
    })() {
        Ok(target) => target,
        Err(e) => return Ok(Observed::Unreachable(format!("{e:#}"))),
    };
    if rec.qroot.is_none() {
        rec.qroot = Some(target.client.qroot.clone());
        save(rec)?;
    }
    // Every failure below a parsed report is the same verdict here:
    // Unreachable carries the reason, whatever its kind.
    let rep = match remote::remote_status(t, &target) {
        remote::StatusAnswer::Report { report, .. } => report,
        remote::StatusAnswer::Transport(e) | remote::StatusAnswer::ConnDown(e) | remote::StatusAnswer::Drift(e) => return Ok(Observed::Unreachable(format!("{e:#}"))),
    };
    let name = &rec.queue_name;
    let obs = if rep.pending.iter().any(|n| n == name) {
        Observed::Pending
    } else if rep.running.iter().any(|n| n == name) {
        Observed::Running
    } else if let Some(r) = rep.done_recent.iter().find(|r| &r.name == name) {
        Observed::Done(r.rc)
    } else if let Some(r) = rep.failed_recent.iter().find(|r| &r.name == name) {
        Observed::Failed(r.rc)
    } else {
        Observed::Unknown
    };
    let (state, rc) = match obs {
        Observed::Done(rc) => ("done", rc),
        Observed::Failed(rc) => ("failed", rc),
        _ => return Ok(obs),
    };
    rec.outcome = Some(TaskOutcome { state: state.into(), rc, observed_at: now_iso() });
    // Event BEFORE the outcome save: the outcome is write-once, so
    // save-then-append would permanently lose this line if the
    // append failed — a re-observe never retries once `outcome` is
    // persisted. With this order a save failure just means the next
    // `task show` re-observes and re-appends (a duplicate finish
    // line beats a missing one).
    append_event(serde_json::json!({
        "event": if state == "done" { "finish" } else { "fail" },
        "id": rec.id, "client": rec.client, "queue": rec.queue_name,
        "target_queue": rec.queue, "qroot": rec.qroot, "rc": rc,
    }))?;
    save(rec)?;
    Ok(obs)
}

/// `client rename` keeps ledger records pointing at the live row
/// (the same rule spool entries follow). Best-effort per record.
pub fn retarget_client(old: &str, new: &str) {
    if let Ok(mut recs) = list() {
        for rec in &mut recs {
            if rec.client == old {
                rec.client = new.to_string();
                if let Err(e) = save(rec) {
                    eprintln!("tasks: cannot retarget {}: {e:#}", rec.id);
                }
            }
        }
    }
}

/// Raw event lines for `task events`; empty when nothing recorded.
pub fn events() -> Result<String> {
    match fs::read_to_string(events_path()?) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e).with_context(|| "cannot read events.jsonl"),
    }
}
