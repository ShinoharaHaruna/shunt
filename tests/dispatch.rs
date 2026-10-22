//! Dispatcher integration: two-phase ack → delete, failure → retain.
//! Uses a fake transport that appends the stdin body into a local tmp
//! `pending/` (mimicking `shunt enqueue -`) or returns canned
//! failures. No network.

use anyhow::Result;
use shunt::proto::task::{SpoolEntry, TaskMeta};
use shunt::server::dispatch;
use shunt::server::registry::{self, ClientEntry, SshEndpoint};
use shunt::server::spool;
use shunt::server::transport::Transport;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Fakes a box: `status --json` reports daemon alive, `enqueue -`
/// appends the body to `<pending>` and acks. `fail_enqueue` simulates
/// a remote enqueue error; `down` simulates unreachable ssh.
struct FakeTransport {
    pending: PathBuf,
    fail_enqueue: bool,
    down: bool,
    /// Extra `done_recent` rows in the `status` answer, for the
    /// ledger's lazy-outcome path.
    done_recent: Vec<(String, i32)>,
    /// `status` replies with non-wire garbage (protocol drift).
    drift: bool,
    /// `daemon.alive` in the `status` answer; false exercises the
    /// auto-heal `start` path.
    daemon_alive: bool,
    /// When set, `enqueue` returns this raw wire line instead of the
    /// normal `{ok,name}` ack — exercises malformed-ack handling.
    ack_line: Option<String>,
    calls: Mutex<Vec<String>>,
}

fn fake(pending: &Path) -> FakeTransport {
    FakeTransport { pending: pending.to_path_buf(), fail_enqueue: false, down: false, done_recent: Vec::new(), drift: false, daemon_alive: true, ack_line: None, calls: Mutex::new(Vec::new()) }
}

impl Transport for FakeTransport {
    fn exec(&self, client: &ClientEntry, argv: &[String], stdin: Option<&str>) -> Result<String> {
        self.calls.lock().unwrap().push(argv.get(1).cloned().unwrap_or_default());
        if self.down {
            anyhow::bail!("ssh connect timeout");
        }
        match argv.get(1).map(String::as_str) {
            Some("status") => {
                if self.drift {
                    return Ok("shunt v9.9 ready\n".into());
                }
                let done: Vec<String> = self.done_recent.iter().map(|(n, rc)| format!(r#"{{"name":"{n}","rc":{rc}}}"#)).collect();
                // Wire answers are one compact JSON object line
                // (design.md §5); extract() line-scans for it.
                Ok(format!(r#"{{"protocol":1,"daemon":{{"alive":{},"pid":1234}},"workers":{{"value":3,"source":"default"}},"running":[],"pending":[],"done_recent":[{}],"failed_recent":[],"stop":false,"shutdown":{{"armed":false,"grace_s":60}}}}"#, self.daemon_alive, done.join(",")))
            }
            Some("enqueue") => {
                if self.fail_enqueue {
                    anyhow::bail!("remote rc=1: disk full");
                }
                let body = stdin.unwrap_or("");
                let name = "0100_task";
                let dir = self.pending.join(name);
                std::fs::create_dir_all(&dir)?;
                std::fs::write(dir.join("task.sh"), body)?;
                if let Some(line) = &self.ack_line {
                    return Ok(format!("{line}\n"));
                }
                Ok(format!(r#"{{"protocol":1,"ok":true,"name":"{name}","client":"{}"}}"#, client.name))
            }
            _ => Ok(r#"{"protocol":1,"ok":true}"#.into()),
        }
    }

    fn copy_to(&self, _client: &ClientEntry, _local: &Path, _remote: &str) -> Result<()> {
        Ok(())
    }
}

/// Serialize tests that mutate process env (SHUNT_HOME).
static HOME_LOCK: Mutex<()> = Mutex::new(());

fn fixture(tag: &str) -> Result<(PathBuf, ClientEntry)> {
    let home = std::env::temp_dir().join(format!("shunt-itest-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&home)?;
    std::env::set_var("SHUNT_HOME", &home);
    let client = ClientEntry { name: "fake-a".into(), local: false, disabled: false, local_bin: None, bin_path: None, timeout_s: None, ssh_timeout_s: None, shutdown_cmd: None, expire: String::new(), env_setup: String::new(), shell: String::new(), endpoint_rotates: false, shared: false, exit_on_idle: false, ssh: SshEndpoint { host: "127.0.0.1".into(), port: 22, user: "root".into(), key: String::new(), password: String::new() }, qroot: "/root/queue".into(), deploy_root: "/root".into(), facts: BTreeMap::new(), billing: None };
    registry::save_client(&client)?;
    Ok((home, client))
}

fn entry(id: &str, slug: &str) -> SpoolEntry {
    SpoolEntry { id: id.into(), project: "p".into(), client: "fake-a".into(), meta: TaskMeta { id: None, slug: slug.into(), guard: None, workdir: None, env: BTreeMap::new(), cmd: vec!["echo".into(), "hi".into()], timeout_s: None }, submitted_at: "2024-01-01T00:00:00Z".into() }
}

#[test]
fn dispatch_ack_deletes_failure_retains() -> Result<()> {
    let _lock = HOME_LOCK.lock().unwrap();
    let (home, _client) = fixture("ack")?;
    let pending = home.join("pending");
    std::fs::create_dir_all(&pending)?;

    // Clean ack: pushed and removed from spool.
    spool::stage(&entry("t1", "a-t1"), "echo t1\n")?;
    let t = fake(&pending);
    assert_eq!(dispatch::sweep_with(&t)?, 1);
    assert!(spool::list()?.is_empty());
    assert_eq!(std::fs::read_to_string(pending.join("0100_task/task.sh"))?, "echo t1\n");

    // Ledger (design.md §4.6): the ack recorded spool id -> queue
    // name, and a `pushed` line landed in events.jsonl.
    let rec = shunt::server::tasks::load("t1")?;
    assert_eq!(rec.queue_name, "0100_task");
    assert_eq!(rec.client, "fake-a");
    assert!(rec.outcome.is_none());
    let ev = shunt::server::tasks::events()?;
    assert!(ev.contains("\"event\":\"pushed\"") && ev.contains("\"id\":\"t1\""), "{ev}");

    // Remote enqueue error: entry stays for the next poll.
    spool::stage(&entry("t2", "a-t2"), "echo t2\n")?;
    let t_fail = FakeTransport { fail_enqueue: true, ..fake(&pending) };
    assert_eq!(dispatch::sweep_with(&t_fail)?, 0);
    assert_eq!(spool::list()?.len(), 1);

    // Unreachable box: entry stays, no error.
    spool::stage(&entry("t3", "a-t3"), "echo t3\n")?;
    let t_down = FakeTransport { down: true, ..fake(&pending) };
    assert_eq!(dispatch::sweep_with(&t_down)?, 0);
    assert_eq!(spool::list()?.len(), 2);

    std::fs::remove_dir_all(&home).ok();
    Ok(())
}

/// A client whose conn_fail streak hit CONN_FAIL_ESCALATE is probed
/// only every PROBE_BACKOFF_EVERY sweeps — dead boxes stop costing a
/// connect timeout on every poll (head-of-line fix).
#[test]
fn dead_box_probe_backs_off() -> Result<()> {
    let _lock = HOME_LOCK.lock().unwrap();
    let (home, _client) = fixture("backoff")?;
    let pending = home.join("pending");
    std::fs::create_dir_all(&pending)?;

    spool::stage(&entry("t1", "a-t1"), "echo t1\n")?;
    for _ in 0..registry::CONN_FAIL_ESCALATE {
        registry::conn_fail_bump("fake-a");
    }

    let t = fake(&pending);
    // Sweeps 1..=3 skip the probe entirely: no exec, entry retained.
    for idx in 1..4 {
        assert_eq!(dispatch::sweep_at(&t, idx)?, 0);
    }
    assert!(t.calls.lock().unwrap().is_empty());
    assert_eq!(spool::list()?.len(), 1);

    // Sweep 4 probes again; the box is back, delivery proceeds.
    assert_eq!(dispatch::sweep_at(&t, 4)?, 1);
    assert!(spool::list()?.is_empty());

    std::fs::remove_dir_all(&home).ok();
    Ok(())
}

/// A corrupt spool file must not wedge the sweep: it is quarantined
/// to `*.bad` and the healthy entry still dispatches.
#[test]
fn bad_spool_entry_quarantined_good_delivers() -> Result<()> {
    let _lock = HOME_LOCK.lock().unwrap();
    let (home, _client) = fixture("bad")?;
    let pending = home.join("pending");
    std::fs::create_dir_all(&pending)?;

    // One corrupt entry (garbage JSON) and one healthy entry.
    let dir = spool::spool_dir()?;
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("zzz_corrupt.json"), "{not json")?;
    std::fs::write(dir.join("zzz_corrupt.sh"), "echo corrupt\n")?;
    spool::stage(&entry("t1", "a-t1"), "echo t1\n")?;

    let t = fake(&pending);
    assert_eq!(dispatch::sweep_with(&t)?, 1);

    // Healthy entry delivered and gone; corrupt files renamed aside.
    assert!(spool::list()?.is_empty());
    assert_eq!(std::fs::read_to_string(pending.join("0100_task/task.sh"))?, "echo t1\n");
    assert!(dir.join("zzz_corrupt.json.bad").exists());
    assert!(dir.join("zzz_corrupt.sh.bad").exists());
    assert!(!dir.join("zzz_corrupt.json").exists());

    std::fs::remove_dir_all(&home).ok();
    Ok(())
}
/// A clean `{ok:true}` ack whose `name` is missing, empty, or not
/// `NNNN_<slug>` is a failed push: the spool entry is retained and
/// no ledger record is written (a `queue_name:""` entry would
/// report `unknown` forever, design.md §4.6).
#[test]
fn malformed_ack_retains_entry_and_skips_ledger() -> Result<()> {
    let _lock = HOME_LOCK.lock().unwrap();
    let (home, _client) = fixture("badack")?;
    let pending = home.join("pending");
    std::fs::create_dir_all(&pending)?;

    for (id, line) in [
        ("b1", r#"{"protocol":1,"ok":true}"#),                  // name missing
        ("b2", r#"{"protocol":1,"ok":true,"name":""}"#),        // name empty
        ("b3", r#"{"protocol":1,"ok":true,"name":"zzz !!!"}"#), // not NNNN_<slug>
        ("b4", r#"{"protocol":1,"ok":true,"name":"12345_x"}"#), // 5-digit seq
    ] {
        spool::stage(&entry(id, &format!("a-{id}")), "echo hi\n")?;
        let t = FakeTransport { ack_line: Some(line.into()), ..fake(&pending) };
        assert_eq!(dispatch::sweep_with(&t)?, 0, "{line}");
        assert!(shunt::server::tasks::load(id).is_err(), "ledger must not record {line}");
    }
    assert_eq!(spool::list()?.len(), 4, "all bad-ack entries retained for retry");

    std::fs::remove_dir_all(&home).ok();
    Ok(())
}

/// `task show`'s lazy outcome resolution (§4.6): a terminal
/// sighting in `done_recent` is written back to the record and
/// appended to events.jsonl exactly once; transient/absent states
/// report live but record nothing.
#[test]
fn task_ledger_lazy_outcome() -> Result<()> {
    use shunt::server::tasks::{self, Observed};
    let _lock = HOME_LOCK.lock().unwrap();
    let (home, client) = fixture("ledger")?;
    let pending = home.join("pending");
    std::fs::create_dir_all(&pending)?;

    let e = entry("t9", "a-t9");
    tasks::record_push(&e, "0100_task")?;

    // Not yet on any track list -> unknown, nothing recorded.
    let t = fake(&pending);
    let mut rec = tasks::load("t9")?;
    let obs = tasks::observe(&t, &client, &mut rec)?;
    assert!(matches!(obs, Observed::Unknown), "{obs:?}");
    assert!(tasks::load("t9")?.outcome.is_none());

    // Now it lands in done_recent: written back + one finish event.
    let t_done = FakeTransport { done_recent: vec![("0100_task".into(), 0)], ..fake(&pending) };
    let mut rec = tasks::load("t9")?;
    assert!(matches!(tasks::observe(&t_done, &client, &mut rec)?, Observed::Done(0)));
    let rec = tasks::load("t9")?;
    let o = rec.outcome.unwrap();
    assert_eq!(o.state, "done");
    assert_eq!(o.rc, 0);
    let ev = tasks::events()?;
    assert_eq!(ev.matches("\"event\":\"finish\"").count(), 1, "{ev}");

    // A recorded outcome answers without another wire call.
    let mut rec = tasks::load("t9")?;
    assert!(matches!(tasks::observe(&t, &client, &mut rec)?, Observed::Done(0)));
    assert_eq!(tasks::events()?.matches("\"event\":").count(), 2); // pushed + finish

    std::fs::remove_dir_all(&home).ok();
    Ok(())
}

/// Ledger traversal guard (same rule as spool::read_entry): a
/// planted tasks/*.json whose inner id is unsafe or mismatched is
/// skipped by list() and refused by load(), and save() refuses a
/// bad id outright — no path may re-join it outside tasks/.
#[test]
fn task_ledger_rejects_traversal_id() -> Result<()> {
    use shunt::server::tasks;
    let _lock = HOME_LOCK.lock().unwrap();
    let (home, _c) = fixture("ledgertrav")?;
    let dir = home.join("tasks");
    std::fs::create_dir_all(&dir)?;

    let planted = serde_json::json!({
        "id": "../escape", "project": "p", "client": "fake-a",
        "slug": "s", "queue_name": "q",
        "submitted_at": "t", "pushed_at": "t",
    });
    std::fs::write(dir.join("x.json"), planted.to_string())?;

    // list() skips it — it must never reach retarget_client/save.
    assert!(tasks::list()?.is_empty());
    // load() reads x.json but refuses the mismatched inner id.
    assert!(tasks::load("x").is_err());
    // save() refuses a bad id from any caller (record_push path).
    assert!(tasks::record_push(&entry("../evil", "s"), "q").is_err());
    // retarget_client over the planted file writes nothing outside.
    tasks::retarget_client("fake-a", "renamed");
    assert!(!home.join("escape.json").exists());

    std::fs::remove_dir_all(&home).ok();
    Ok(())
}

/// Auto-heal (§4.4): ssh answers but the daemon is dead — the
/// sweep issues a remote `start` before pushing the entry.
#[test]
fn dead_daemon_is_restarted_then_pushed() -> Result<()> {
    let _lock = HOME_LOCK.lock().unwrap();
    let (home, _client) = fixture("heal")?;
    let pending = home.join("pending");
    std::fs::create_dir_all(&pending)?;

    spool::stage(&entry("t1", "a-t1"), "echo t1\n")?;
    let t = FakeTransport { daemon_alive: false, ..fake(&pending) };
    assert_eq!(dispatch::sweep_with(&t)?, 1);
    let calls = t.calls.lock().unwrap();
    assert_eq!(calls.as_slice(), ["status", "start", "enqueue"], "{calls:?}");
    drop(calls);
    assert!(spool::list()?.is_empty());
    assert_eq!(std::fs::read_to_string(pending.join("0100_task/task.sh"))?, "echo t1\n");

    std::fs::remove_dir_all(&home).ok();
    Ok(())
}

/// `submit --file` injects the binding's workdir/env directives into
/// the file's comment head (same contract as `submit -- cmd`), and
/// an explicit `--guard` lands as a guard line ahead of the body.
#[test]
fn submit_file_merges_binding_directives() -> Result<()> {
    let _lock = HOME_LOCK.lock().unwrap();
    let (home, _c) = fixture("filemerge")?;
    registry::save_project(&registry::ProjectEntry { name: "p".into(), desc: String::new(), repo: String::new(), defaults: Default::default(), bind: vec![registry::Binding { client: "fake-a".into(), workdir: "/bound/wd".into(), env: BTreeMap::from([("K".into(), "bound".into())]) }] })?;

    let f = home.join("t.sh");
    std::fs::write(&f, "#!/usr/bin/env bash\necho hi\n")?;
    let id = spool::submit("fake-a", Some("p"), Some("runs/g.json"), None, Some(f.to_str().unwrap()), None, &[])?;
    let body = spool::body(&id)?;
    let head_end = body.find("echo hi").unwrap();
    let head = &body[..head_end];
    assert!(head.contains("# shunt:workdir=/bound/wd\n"), "{body}");
    assert!(head.contains("# shunt-env:K=bound\n"), "{body}");
    assert!(head.contains("if [ -e runs/g.json ]; then"), "{body}");
    Ok(())
}

/// File-local directives beat binding-injected ones per key: the
/// binding's lines land above the file's own in the comment head, so
/// the runner's last-one-wins scan picks the file's value.
#[test]
fn submit_file_own_directives_win() -> Result<()> {
    let _lock = HOME_LOCK.lock().unwrap();
    let (home, _c) = fixture("filewin")?;
    registry::save_project(&registry::ProjectEntry { name: "p".into(), desc: String::new(), repo: String::new(), defaults: Default::default(), bind: vec![registry::Binding { client: "fake-a".into(), workdir: "/bound/wd".into(), env: BTreeMap::from([("K".into(), "bound".into()), ("K2".into(), "b2".into())]) }] })?;

    let f = home.join("t.sh");
    std::fs::write(&f, "#!/bin/sh\n# shunt:workdir=/own/wd\n# shunt-env:K=own\nset -e\necho hi\n")?;
    let id = spool::submit("fake-a", Some("p"), None, None, Some(f.to_str().unwrap()), None, &[])?;
    let body = spool::body(&id)?;
    // Binding workdir/K suppressed; K2 merged in above the file's lines.
    assert_eq!(body.matches("# shunt:workdir=").count(), 1, "{body}");
    assert!(body.contains("# shunt:workdir=/own/wd"));
    assert_eq!(body.matches("# shunt-env:K=").count(), 1, "{body}");
    assert!(body.contains("# shunt-env:K=own"));
    let k2 = body.find("# shunt-env:K2=b2").unwrap();
    let own = body.find("# shunt-env:K=own").unwrap();
    assert!(k2 < own, "injected binding lines must precede file's own: {body}");
    Ok(())
}

/// A dash-leading slug must be refused at submit time: on the wire it
/// parses as a flag, the remote rejects it, and the spool entry would
/// retry forever.
#[test]
fn submit_rejects_dash_slug() -> Result<()> {
    let _lock = HOME_LOCK.lock().unwrap();
    let (_home, _c) = fixture("dashslug")?;
    let e = spool::submit("fake-a", None, None, Some("-rf"), None, None, &["echo".into(), "hi".into()]);
    assert!(e.is_err(), "dash slug spooled");
    assert!(e.unwrap_err().to_string().contains("bad slug"));
    assert!(spool::list()?.is_empty(), "rejected submit must not stage");
    Ok(())
}

/// A `status` reply that fails wire::parse is protocol drift, not a
/// dead daemon: it must NOT trigger the auto-heal `start` (which
/// would run the drifted binary) — the client is treated as Down for
/// the sweep and the spool entry is retained.
#[test]
fn drifted_status_skips_start_and_push() -> Result<()> {
    let _lock = HOME_LOCK.lock().unwrap();
    let (home, _client) = fixture("drift")?;
    let pending = home.join("pending");
    std::fs::create_dir_all(&pending)?;

    spool::stage(&entry("t1", "a-t1"), "echo t1\n")?;
    let t = FakeTransport { drift: true, ..fake(&pending) };
    assert_eq!(dispatch::sweep_with(&t)?, 0);
    let calls = t.calls.lock().unwrap();
    assert_eq!(calls.as_slice(), ["status"], "probe only — no start, no enqueue");
    drop(calls);
    assert_eq!(spool::list()?.len(), 1, "entry retained for the next sweep");

    std::fs::remove_dir_all(&home).ok();
    Ok(())
}
