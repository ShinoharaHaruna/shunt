//! Multi-queue end-to-end over the loopback transport: one client owns
//! `train` + `eval` qroots, daemons, sequence allocators, and ledgers.
//! Everything runs inside one tmp $SHUNT_HOME; no ssh, no remote box.

use anyhow::Result;
use shunt::proto::task::TaskFlags;
use shunt::server::transport::{queue_exec, LocalTransport};
use shunt::server::{dispatch, registry, spool};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

// SHUNT_HOME is process-global; tests in this binary run serially
// under this lock (same discipline as tests/remove.rs).
static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct Sandbox {
    root: PathBuf,
    _home: std::sync::MutexGuard<'static, ()>,
}
impl Sandbox {
    fn new() -> Result<Self> {
        let home = HOME_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!("shunt-mq-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos()));
        std::fs::create_dir_all(&root)?;
        std::env::set_var("SHUNT_HOME", &root);
        std::env::set_var("POLL", "0.2");
        Ok(Self { root, _home: home })
    }
    fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        // Two daemons may be live; kill them by pidfile before deleting.
        for name in ["train", "eval", "default"] {
            let pidfile = self.path(&format!("q/{name}/daemon.pid"));
            if let Ok(s) = std::fs::read_to_string(&pidfile) {
                if let Ok(pid) = s.trim().parse::<i32>() {
                    let _ = std::process::Command::new("kill").args(["-9", &pid.to_string()]).status();
                }
            }
        }
        std::env::remove_var("SHUNT_HOME");
        std::env::remove_var("POLL");
        std::fs::remove_dir_all(&self.root).ok();
    }
}

fn transport() -> LocalTransport {
    LocalTransport { bin: PathBuf::from(env!("CARGO_BIN_EXE_shunt")), timeout_s: Some(60) }
}

fn wire(t: &LocalTransport, name: &str, tail: &[&str]) -> Result<String> {
    let resolved = registry::load_queue("box", Some(name))?;
    queue_exec(t, &resolved, tail, None)
}

/// One local client carrying `train` and `eval` queues.
fn client(home: &Sandbox) -> Result<registry::ClientEntry> {
    let mut c = registry::ClientEntry { name: "box".into(), local: true, disabled: false, local_bin: Some(env!("CARGO_BIN_EXE_shunt").into()), bin_path: None, timeout_s: Some(60), ssh_timeout_s: None, shutdown_cmd: None, expire: "unlimited".into(), env_setup: String::new(), shell: String::new(), endpoint_rotates: false, shared: false, exit_on_idle: false, ssh: Default::default(), qroot: String::new(), deploy_root: home.root.display().to_string(), facts: Default::default(), billing: None, queues: Default::default(), default_queue: None };
    for name in ["train", "eval"] {
        c.queues.insert(name.into(), registry::QueueEntry { qroot: home.path(&format!("q/{name}")).display().to_string(), env_setup: String::new(), shell: String::new(), exit_on_idle: false, workers: None });
    }
    c.default_queue = Some("train".into());
    Ok(c)
}

fn wait_until(deadline_s: u64, what: &str, mut ready: impl FnMut() -> bool) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(deadline_s);
    while !ready() {
        anyhow::ensure!(Instant::now() < deadline, "{what} not ready within {deadline_s}s");
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

#[test]
fn two_queues_run_independently_on_one_client() -> Result<()> {
    let home = Sandbox::new()?;
    let client = client(&home)?;
    registry::save_client(&client)?;
    let t = transport();

    // Bootstrap-equivalent bring-up: init both qroots over the wire.
    for name in ["train", "eval"] {
        wire(&t, name, &["init", "--json"])?;
    }
    assert!(home.path("q/train/pending").is_dir());
    assert!(home.path("q/eval/pending").is_dir());

    // Independent sequence allocators: enqueue directly into eval.
    wire(&t, "eval", &["enqueue", "--json", "--slug", "seed", "-"])?;
    // `submit` resolves the configured default (train) — staged target pins queue+qroot.
    let flags = TaskFlags::default();
    spool::submit_to("box", None, None, None, Some("to-train"), None, &flags, None, &["touch".into(), "train-hit".into()])?;
    let staged = spool::list()?;
    assert_eq!(staged[0].queue, "train");
    assert_eq!(staged[0].qroot.as_deref(), Some(home.path("q/train").to_str().unwrap()));

    // And an explicit queue target beats the configured default.
    spool::submit_to("box", Some("eval"), None, None, Some("to-eval"), None, &flags, None, &["touch".into(), "eval-hit".into()])?;
    let staged = spool::list()?;
    assert_eq!(staged[1].queue, "eval");

    // One sweep delivers both — each into its own pending/, with its
    // own seq space (the eval enqueue seeded .seq independently).
    dispatch::sweep_with(&t)?;
    assert!(spool::list()?.is_empty());
    let ledger = shunt::server::tasks::list()?;
    assert_eq!(ledger.len(), 2);
    assert_eq!(ledger.iter().map(|r| r.queue.as_str()).collect::<Vec<_>>(), ["train", "eval"]);
    assert_eq!(ledger[0].qroot.as_deref(), Some(home.path("q/train").to_str().unwrap()));

    // Both daemons (auto-started by the sweep) claim and run their tasks.
    wait_until(30, "train task", || home.path("q/train/train-hit").exists())?;
    wait_until(30, "eval task", || home.path("q/eval/eval-hit").exists())?;
    wait_until(30, "train done entry", || home.path("q/train/done").read_dir().map(|mut d| d.next().is_some()).unwrap_or(false))?;
    wait_until(30, "eval done entries", || home.path("q/eval/done").read_dir().map(|d| d.count() >= 2).unwrap_or(false))?;

    // Each queue reports its own daemon over the wire.
    for name in ["train", "eval"] {
        let out = wire(&t, name, &["status", "--json"])?;
        let report: shunt::proto::wire::StatusReport = shunt::proto::wire::parse(&out)?;
        assert!(report.daemon.alive, "{name} daemon should be up");
    }

    // Pidfiles prove two distinct daemons.
    let p1 = std::fs::read_to_string(home.path("q/train/daemon.pid"))?;
    let p2 = std::fs::read_to_string(home.path("q/eval/daemon.pid"))?;
    assert_ne!(p1.trim(), p2.trim());

    for name in ["train", "eval"] {
        wire(&t, name, &["stop", "--json"])?;
    }
    wait_until(10, "daemon exit", || !home.path("q/train/daemon.pid").exists() && !home.path("q/eval/daemon.pid").exists())?;
    Ok(())
}

#[test]
fn changing_default_never_retarguess_staged_tasks() -> Result<()> {
    let home = Sandbox::new()?;
    let mut c = client(&home)?;
    registry::save_client(&c)?;
    let t = transport();
    for name in ["train", "eval"] {
        wire(&t, name, &["init", "--json"])?;
    }

    // Stage for train (the default), then flip the default to eval.
    let flags = TaskFlags::default();
    spool::submit_to("box", None, None, None, Some("pinned"), None, &flags, None, &["true".into()])?;
    let staged = spool::list()?;
    assert_eq!(staged[0].queue, "train");
    c.default_queue = Some("eval".into());
    registry::save_client(&c)?;

    // Delivery still lands in train's qroot — the staged snapshot wins.
    dispatch::sweep_with(&t)?;
    wait_until(15, "train pending entry", || home.path("q/train/pending").read_dir().map(|mut d| d.next().is_some()).unwrap_or(false))?;
    assert!(home.path("q/eval/pending").read_dir()?.next().is_none());
    let ledger = shunt::server::tasks::list()?;
    assert_eq!(ledger[0].queue, "train");

    for name in ["train", "eval"] {
        wire(&t, name, &["stop", "--json"])?;
    }
    Ok(())
}

#[test]
fn legacy_default_queue_and_queue_selection_rules() -> Result<()> {
    let home = Sandbox::new()?;

    // A flat legacy row resolves as a literal `default` queue.
    let legacy = registry::ClientEntry { name: "old".into(), local: true, disabled: false, local_bin: None, bin_path: None, timeout_s: None, ssh_timeout_s: None, shutdown_cmd: None, expire: String::new(), env_setup: "export X=1".into(), shell: "zsh".into(), endpoint_rotates: false, shared: false, exit_on_idle: true, ssh: Default::default(), qroot: home.path("legacy-q").display().to_string(), deploy_root: home.root.display().to_string(), facts: Default::default(), billing: None, queues: Default::default(), default_queue: None };
    registry::save_client(&legacy)?;
    let resolved = registry::load_queue("old", None)?;
    assert_eq!(resolved.name, "default");
    assert_eq!(resolved.client.qroot, legacy.qroot);
    assert_eq!(resolved.client.env_setup, "export X=1");
    assert_eq!(resolved.client.shell, "zsh");
    assert!(resolved.client.exit_on_idle);

    // Explicit mutation canonicalizes without losing legacy values.
    registry::add_queue("old", "second", registry::QueueEntry { qroot: home.path("second-q").display().to_string(), ..Default::default() })?;
    let canon = registry::load_client("old")?;
    assert!(canon.qroot.is_empty() && canon.env_setup.is_empty() && canon.shell.is_empty() && !canon.exit_on_idle);
    assert_eq!(canon.queues["default"].qroot, legacy.qroot);
    assert!(canon.queues["default"].exit_on_idle);
    assert_eq!(canon.default_queue.as_deref(), Some("default"));

    // Two queues without a default: unqualified resolution is ambiguous.
    let mut multi = client(&home)?;
    multi.default_queue = None;
    registry::save_client(&multi)?;
    let err = registry::load_queue("box", None).unwrap_err();
    assert!(format!("{err:#}").contains("ambiguous"), "{err:#}");
    // Unknown names never fall back.
    assert!(registry::load_queue("box", Some("nope")).is_err());
    // Explicit selection wins over default.
    multi.default_queue = Some("train".into());
    registry::save_client(&multi)?;
    assert_eq!(registry::load_queue("box", Some("eval"))?.name, "eval");
    assert_eq!(registry::load_queue("box", None)?.name, "train");

    // Old bindings lacking `queue` deserialize as literal "default",
    // never the configured default.
    let binding: registry::Binding = toml::from_str(
        r#"client = "box"
workdir = "/w""#,
    )?;
    assert_eq!(binding.queue, "default");
    Ok(())
}

#[test]
fn queue_rm_refuses_referenced_or_busy_targets() -> Result<()> {
    let home = Sandbox::new()?;
    let c = client(&home)?;
    registry::save_client(&c)?;
    let t = transport();
    for name in ["train", "eval"] {
        wire(&t, name, &["init", "--json"])?;
    }

    // Configured default is protected.
    let err = shunt::server::queue_ops::remove(&t, "box", "train", false).unwrap_err();
    assert!(format!("{err:#}").contains("configured default"), "{err:#}");

    // A project binding on eval protects it until unbound.
    registry::save_project(&registry::ProjectEntry { name: "p".into(), desc: String::new(), repo: String::new(), defaults: Default::default(), bind: vec![registry::Binding { client: "box".into(), queue: "eval".into(), workdir: "/w".into(), env: BTreeMap::new() }] })?;
    let err = shunt::server::queue_ops::remove(&t, "box", "eval", false).unwrap_err();
    assert!(format!("{err:#}").contains("bound to project"), "{err:#}");

    // Unbinding frees the queue; with no work it removes cleanly —
    // and the shared key/state is untouched (local teardown intact).
    let mut p = registry::load_project("p")?;
    p.bind.retain(|b| b.queue != "eval");
    registry::save_project(&p)?;
    shunt::server::queue_ops::remove(&t, "box", "eval", false)?;
    let after = registry::load_client("box")?;
    assert!(!after.queues.contains_key("eval"));
    assert!(after.queues.contains_key("train"));
    assert!(home.path("q/eval").exists(), "no --purge keeps qroot files");

    // train remains the only queue and cannot be removed either.
    let err = shunt::server::queue_ops::remove(&t, "box", "train", false).unwrap_err();
    assert!(format!("{err:#}").contains("last queue"), "{err:#}");
    for name in ["train", "eval"] {
        wire(&t, name, &["stop", "--json"]).ok();
    }
    Ok(())
}
