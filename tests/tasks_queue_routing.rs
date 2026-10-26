//! Named task targets stay stable across defaults, retries, and observations.
//! All delivery operations use a fake transport and an isolated registry.

use anyhow::Result;
use shunt::proto::task::{SpoolEntry, TaskFlags};
use shunt::server::registry::{self, ClientEntry};
use shunt::server::tasks::{self, Observed};
use shunt::server::transport::Transport;
use shunt::server::{dispatch, spool};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::new(());

struct Fixture {
    home: PathBuf,
    old_home: Option<OsString>,
}

impl Fixture {
    fn new(tag: &str, queues: &[&str], default: &str) -> Result<Self> {
        let home = std::env::temp_dir().join(format!("shunt-task-routing-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&home)?;
        let f = Self { home, old_home: std::env::var_os("SHUNT_HOME") };
        std::env::set_var("SHUNT_HOME", &f.home);
        let mut entries = serde_json::Map::new();
        for name in queues {
            entries.insert(
                (*name).into(),
                serde_json::json!({
                    "qroot": f.root(name), "env_setup": "", "shell": "", "exit_on_idle": false,
                }),
            );
        }
        let client: ClientEntry = serde_json::from_value(serde_json::json!({
            "name": "box", "qroot": "", "deploy_root": f.home,
            "ssh": {"host": "example.invalid", "port": 22, "user": "test"},
            "queues": entries, "default_queue": default,
        }))?;
        registry::save_client(&client)?;
        Ok(f)
    }

    fn root(&self, queue: &str) -> String {
        self.home.join(format!("queue-{queue}")).display().to_string()
    }

    fn submit(&self, queue: Option<&str>, project: Option<&str>) -> Result<String> {
        spool::submit_to("box", queue, project, Some("done.marker"), Some("task"), None, &TaskFlags::default(), None, None, &["true".into()])
    }

    fn project(&self, name: &str, queues: &[&str]) -> Result<()> {
        let bind: Vec<_> = queues
            .iter()
            .map(|queue| {
                serde_json::json!({
                    "client": "box", "queue": queue, "workdir": format!("/work/{queue}"),
                    "env": {"TARGET": queue},
                })
            })
            .collect();
        registry::save_project(&serde_json::from_value(serde_json::json!({"name": name, "bind": bind}))?)?;
        Ok(())
    }

    fn change_root(&self, queue: &str, root: &str) -> Result<()> {
        let mut c = registry::load_client("box")?;
        c.queues.get_mut(queue).unwrap().qroot = root.into();
        registry::save_client(&c)?;
        Ok(())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        match &self.old_home {
            Some(home) => std::env::set_var("SHUNT_HOME", home),
            None => std::env::remove_var("SHUNT_HOME"),
        }
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

#[derive(Default)]
struct Fake {
    calls: Mutex<Vec<(String, String)>>,
    dead_daemons: bool,
    broken_root: Option<String>,
    down: bool,
    reject_enqueue: bool,
    done: bool,
}

impl Transport for Fake {
    fn exec(&self, c: &ClientEntry, argv: &[String], _: Option<&str>) -> Result<String> {
        let op = argv[1].as_str();
        assert_eq!(argv[0], format!("{}/bin/shunt", c.qroot));
        let root = argv.iter().position(|arg| arg == "--root").unwrap();
        assert_eq!(argv[root + 1], c.qroot);
        self.calls.lock().unwrap().push((c.qroot.clone(), op.into()));
        if self.down {
            registry::conn_fail_bump(&c.name);
            anyhow::bail!("mock exhausted SSH retries");
        }
        if op == "status" {
            if self.broken_root.as_deref() == Some(&c.qroot) {
                return Ok("not wire JSON".into());
            }
            return Ok(serde_json::json!({
                "protocol": 1, "daemon": {"alive": !self.dead_daemons, "pid": 1234},
                "workers": {"value": 1, "source": "default"},
                "pending": [], "running": [],
                "done_recent": if self.done { vec![serde_json::json!({"name": "0100_task", "rc": 0})] } else { vec![] },
                "failed_recent": [], "stop": false, "shutdown": {"armed": false, "grace_s": 60},
            })
            .to_string());
        }
        if op == "enqueue" {
            anyhow::ensure!(!self.reject_enqueue, "mock lost enqueue ack");
            return Ok(r#"{"protocol":1,"ok":true,"name":"0100_task"}"#.into());
        }
        anyhow::ensure!(op == "start", "unexpected operation {op}");
        Ok(r#"{"protocol":1,"ok":true}"#.into())
    }

    fn copy_to(&self, _: &ClientEntry, _: &Path, _: &str) -> Result<()> {
        anyhow::bail!("unexpected copy")
    }
}

#[test]
fn pair_bindings_and_changed_defaults_keep_concrete_targets() -> Result<()> {
    let _lock = ENV_LOCK.lock().unwrap();
    let f = Fixture::new("pairs", &["alpha", "beta"], "alpha")?;
    f.project("both", &["alpha", "beta"])?;
    f.project("one", &["beta"])?;
    let alpha = f.submit(Some("alpha"), Some("both"))?;
    let beta = f.submit(Some("beta"), Some("both"))?;
    let selected_default = f.submit(None, Some("both"))?;
    let sole_binding = f.submit(None, Some("one"))?;
    assert!(f.submit(Some("alpha"), Some("one")).is_err());
    registry::set_default_queue("box", "beta")?;
    let new_default = f.submit(None, None)?;
    for (id, queue) in [(&alpha, "alpha"), (&beta, "beta"), (&selected_default, "alpha"), (&sole_binding, "beta"), (&new_default, "beta")] {
        let entry = spool::list()?.into_iter().find(|e| &e.id == id).unwrap();
        assert_eq!(entry.queue, queue);
        assert_eq!(entry.qroot.as_deref(), Some(f.root(queue).as_str()));
        if id != &new_default {
            assert!(spool::body(id)?.contains(&format!("# shunt:workdir=/work/{queue}\n")));
        }
    }
    let t = Fake::default();
    assert_eq!(dispatch::sweep_with(&t)?, 5);
    for (id, queue) in [(&alpha, "alpha"), (&beta, "beta"), (&selected_default, "alpha"), (&sole_binding, "beta"), (&new_default, "beta")] {
        let rec = tasks::load(id)?;
        assert_eq!(rec.queue, queue);
        assert_eq!(rec.qroot.as_deref(), Some(f.root(queue).as_str()));
        assert_eq!(rec.queue_name, "0100_task");
    }
    let calls = t.calls.lock().unwrap();
    for queue in ["alpha", "beta"] {
        assert_eq!(calls.iter().filter(|(root, op)| root == &f.root(queue) && op == "status").count(), 1);
    }
    for line in tasks::events()?.lines() {
        let event: serde_json::Value = serde_json::from_str(line)?;
        assert_eq!(event["queue"], "0100_task");
        let queue = event["target_queue"].as_str().unwrap();
        assert_eq!(event["qroot"], f.root(queue));
    }
    Ok(())
}

#[test]
fn sibling_daemons_start_independently_and_bad_queue_is_isolated() -> Result<()> {
    let _lock = ENV_LOCK.lock().unwrap();
    let f = Fixture::new("siblings", &["alpha", "beta"], "alpha")?;
    for queue in ["alpha", "alpha", "beta"] {
        f.submit(Some(queue), None)?;
    }
    let t = Fake { dead_daemons: true, ..Fake::default() };
    assert_eq!(dispatch::sweep_with(&t)?, 3);
    for queue in ["alpha", "beta"] {
        assert_eq!(t.calls.lock().unwrap().iter().filter(|(root, op)| root == &f.root(queue) && op == "start").count(), 1);
    }
    f.submit(Some("alpha"), None)?;
    f.submit(Some("alpha"), None)?;
    f.submit(Some("beta"), None)?;
    let t = Fake { broken_root: Some(f.root("alpha")), dead_daemons: true, ..Fake::default() };
    assert_eq!(dispatch::sweep_with(&t)?, 1);
    assert_eq!(spool::list()?.len(), 2);
    assert_eq!(t.calls.lock().unwrap().iter().filter(|(_, op)| op == "status").count(), 2);
    assert_eq!(t.calls.lock().unwrap().iter().filter(|(_, op)| op == "start").count(), 1);
    Ok(())
}

#[test]
fn machine_connection_failure_is_probed_only_once() -> Result<()> {
    let _lock = ENV_LOCK.lock().unwrap();
    let f = Fixture::new("down", &["alpha", "beta"], "alpha")?;
    for queue in ["alpha", "alpha", "beta"] {
        f.submit(Some(queue), None)?;
    }
    let t = Fake { down: true, ..Fake::default() };
    assert_eq!(dispatch::sweep_with(&t)?, 0);
    assert_eq!(t.calls.lock().unwrap().len(), 1);
    assert_eq!(spool::list()?.len(), 3);
    Ok(())
}

#[test]
fn legacy_spool_pins_literal_default_before_lost_ack_and_rejects_drift() -> Result<()> {
    let _lock = ENV_LOCK.lock().unwrap();
    let f = Fixture::new("legacy", &["default", "beta"], "beta")?;
    let id = f.submit(Some("default"), None)?;
    let path = spool::spool_dir()?.join(format!("{id}.json"));
    let mut json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
    json.as_object_mut().unwrap().remove("queue");
    json.as_object_mut().unwrap().remove("qroot");
    std::fs::write(&path, json.to_string())?;
    let entry: SpoolEntry = serde_json::from_value(json)?;
    assert_eq!(entry.queue, "default");
    let t = Fake { reject_enqueue: true, ..Fake::default() };
    assert_eq!(dispatch::sweep_with(&t)?, 0);
    assert_eq!(spool::list()?[0].qroot.as_deref(), Some(f.root("default").as_str()));
    f.change_root("default", &f.root("moved"))?;
    let t = Fake::default();
    assert_eq!(dispatch::sweep_with(&t)?, 0);
    assert!(t.calls.lock().unwrap().is_empty());
    assert_eq!(spool::list()?.len(), 1);
    f.change_root("default", &f.root("default"))?;
    assert_eq!(dispatch::sweep_with(&t)?, 1);
    let rec = tasks::load(&id)?;
    assert_eq!(rec.queue, "default");
    assert_eq!(rec.qroot.as_deref(), Some(f.root("default").as_str()));
    Ok(())
}

#[test]
fn ledger_observation_uses_recorded_queue_and_rejects_qroot_drift() -> Result<()> {
    let _lock = ENV_LOCK.lock().unwrap();
    let f = Fixture::new("observe", &["default", "beta"], "beta")?;
    let id = f.submit(Some("default"), None)?;
    assert_eq!(dispatch::sweep_with(&Fake::default())?, 1);
    let mut rec = tasks::load(&id)?;
    let t = Fake::default();
    assert!(matches!(tasks::observe(&t, &registry::load_client("box")?, &mut rec)?, Observed::Unknown));
    assert_eq!(t.calls.lock().unwrap()[0].0, f.root("default"));
    f.change_root("default", &f.root("moved"))?;
    let t = Fake { done: true, ..Fake::default() };
    let observation = tasks::observe(&t, &registry::load_client("box")?, &mut rec)?;
    assert!(matches!(observation, Observed::Unreachable(ref reason) if reason.contains("qroot changed")), "{observation:?}");
    assert!(t.calls.lock().unwrap().is_empty());
    assert!(tasks::load(&id)?.outcome.is_none());
    Ok(())
}

#[test]
fn legacy_ledger_defaults_literal_queue_and_pins_its_root() -> Result<()> {
    let _lock = ENV_LOCK.lock().unwrap();
    let f = Fixture::new("old-ledger", &["default", "beta"], "beta")?;
    let id = f.submit(Some("default"), None)?;
    assert_eq!(dispatch::sweep_with(&Fake::default())?, 1);
    let path = f.home.join(format!("tasks/{id}.json"));
    let mut json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
    json.as_object_mut().unwrap().remove("queue");
    json.as_object_mut().unwrap().remove("qroot");
    std::fs::write(path, json.to_string())?;
    let mut rec = tasks::load(&id)?;
    assert_eq!(rec.queue, "default");
    let t = Fake { done: true, ..Fake::default() };
    assert!(matches!(tasks::observe(&t, &registry::load_client("box")?, &mut rec)?, Observed::Done(0)));
    assert_eq!(tasks::load(&id)?.qroot.as_deref(), Some(f.root("default").as_str()));
    assert_eq!(t.calls.lock().unwrap()[0].0, f.root("default"));
    let event: serde_json::Value = serde_json::from_str(tasks::events()?.lines().last().unwrap())?;
    assert_eq!(event["queue"], "0100_task");
    assert_eq!(event["target_queue"], "default");
    assert_eq!(event["qroot"], f.root("default"));
    Ok(())
}
