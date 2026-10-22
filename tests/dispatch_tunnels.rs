//! Recorded tunnels remain a dispatch responsibility after the spool drains.
//! All remote operations are fake; tunnel setup uses a PATH-scoped mock ssh.

use anyhow::Result;
use shunt::proto::task::{SpoolEntry, TaskMeta};
use shunt::server::registry::{self, ClientEntry, SshEndpoint};
use shunt::server::transport::Transport;
use shunt::server::{dispatch, spool, tunnel};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::new(());

struct Fixture {
    home: PathBuf,
    old_home: Option<OsString>,
    old_path: Option<OsString>,
}

impl Fixture {
    fn new(tag: &str) -> Result<Self> {
        let home = std::env::temp_dir().join(format!("shunt-dispatch-tunnels-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(home.join("mock-bin"))?;
        let f = Self { home, old_home: std::env::var_os("SHUNT_HOME"), old_path: std::env::var_os("PATH") };
        std::env::set_var("SHUNT_HOME", &f.home);
        let mut paths = vec![f.home.join("mock-bin")];
        paths.extend(std::env::split_paths(&f.old_path.clone().unwrap_or_default()));
        std::env::set_var("PATH", std::env::join_paths(paths)?);
        f.mock_ssh(false)?;
        Ok(f)
    }

    fn mock_ssh(&self, succeeds: bool) -> Result<()> {
        let action = if succeeds {
            let status = Command::new("/usr/bin/mkfifo").arg(self.home.join("ssh-hold")).status()?;
            anyhow::ensure!(status.success(), "create mock ssh hold FIFO");
            // A builtin read on an open read/write FIFO holds one process,
            // with no network traffic or grandchildren to leak on cleanup.
            "exec -a ssh /bin/bash -c 'while read -r _; do :; done <> \"$SHUNT_HOME/ssh-hold\"' -- \"$@\""
        } else {
            "exit 1"
        };
        let path = self.home.join("mock-bin/ssh");
        std::fs::write(&path, format!("#!/bin/bash\nprintf '%s\\n' \"$$\" >> \"$SHUNT_HOME/ssh-attempts\"\n{action}\n"))?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
        Ok(())
    }

    fn client(&self, name: &str, local: bool) -> Result<ClientEntry> {
        let c = ClientEntry { name: name.into(), local, disabled: false, local_bin: None, bin_path: None, timeout_s: None, ssh_timeout_s: None, shutdown_cmd: None, expire: String::new(), env_setup: String::new(), shell: String::new(), endpoint_rotates: false, shared: false, exit_on_idle: false, ssh: SshEndpoint { host: "example.invalid".into(), port: 22, user: "test".into(), key: String::new(), password: String::new() }, qroot: self.home.join(format!("queue-{name}")).display().to_string(), deploy_root: self.home.display().to_string(), facts: BTreeMap::new(), billing: None, queues: BTreeMap::new(), default_queue: None };
        registry::save_client(&c)?;
        Ok(c)
    }

    fn record(&self, name: &str, pid: bool) -> Result<()> {
        std::fs::create_dir_all(self.home.join("tunnels"))?;
        std::fs::write(self.home.join(format!("tunnels/{name}.port")), "2080\n")?;
        if pid {
            std::fs::write(self.home.join(format!("tunnels/{name}.pid")), "99999999\n")?;
        }
        Ok(())
    }

    fn attempts(&self) -> usize {
        std::fs::read_to_string(self.home.join("ssh-attempts")).unwrap_or_default().lines().count()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Ok(pids) = std::fs::read_to_string(self.home.join("ssh-attempts")) {
            for pid in pids.lines() {
                // Only signal a live mock we own, never a recycled pid.
                let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
                if cmdline.starts_with(b"ssh\0") && String::from_utf8_lossy(&cmdline).contains(self.home.to_str().unwrap()) {
                    let _ = Command::new("/bin/kill").args(["-TERM", "--", &format!("-{pid}")]).status();
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                    while Path::new(&format!("/proc/{pid}")).exists() && std::time::Instant::now() < deadline {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                }
            }
        }
        for (key, value) in [("SHUNT_HOME", &self.old_home), ("PATH", &self.old_path)] {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

#[derive(Default)]
struct Fake {
    calls: Mutex<Vec<(String, String)>>,
    deliveries: Mutex<Vec<String>>,
    down: bool,
    daemon_dead: bool,
    drift: bool,
}

impl Transport for Fake {
    fn exec(&self, c: &ClientEntry, argv: &[String], stdin: Option<&str>) -> Result<String> {
        let op = if argv[0] == "sh" { "env" } else { &argv[1] };
        self.calls.lock().unwrap().push((c.name.clone(), op.into()));
        anyhow::ensure!(!self.down, "mock box is off");
        match op {
            "status" if self.drift => Ok("not wire JSON".into()),
            "status" => Ok(serde_json::json!({
                "protocol": 1, "daemon": {"alive": !self.daemon_dead, "pid": 1234},
                "workers": {"value": 1, "source": "default"},
                "pending": ["0002_waiting"], "running": ["0001_busy"],
                "done_recent": [], "failed_recent": [], "stop": false,
                "shutdown": {"armed": false, "grace_s": 60}
            })
            .to_string()),
            "env" => {
                let out = Command::new("/bin/sh").args(&argv[1..]).output()?;
                anyhow::ensure!(out.status.success(), "mock env edit failed: {out:?}");
                Ok(String::new())
            }
            "enqueue" => {
                self.deliveries.lock().unwrap().push(stdin.unwrap().into());
                Ok(r#"{"protocol":1,"ok":true,"name":"0100_task"}"#.into())
            }
            "start" => Ok(r#"{"protocol":1,"ok":true}"#.into()),
            _ => anyhow::bail!("unsolicited operation: {argv:?}"),
        }
    }

    fn copy_to(&self, _: &ClientEntry, _: &Path, _: &str) -> Result<()> {
        anyhow::bail!("unsolicited copy")
    }
}

fn calls(t: &Fake) -> Vec<(String, String)> {
    t.calls.lock().unwrap().clone()
}

#[test]
fn empty_spool_dead_tunnel_strips_proxy_and_retries() -> Result<()> {
    let _lock = ENV_LOCK.lock().unwrap();
    let f = Fixture::new("strip")?;
    let c = f.client("remote", false)?;
    f.record(&c.name, true)?;
    std::fs::create_dir_all(&c.qroot)?;
    let env = Path::new(&c.qroot).join("env");
    let t = Fake::default();
    for sweep in 1..=3 {
        std::fs::write(&env, "KEEP=value\nALL_PROXY=socks5h://127.0.0.1:2080\n")?;
        assert_eq!(dispatch::sweep_with(&t)?, 0);
        assert_eq!(calls(&t).len(), 2 * sweep, "one probe and one env repair per sweep");
        assert_eq!(std::fs::read_to_string(&env)?, "KEEP=value\n");
        assert_eq!(f.attempts(), sweep, "one reconnect attempt per sweep");
        assert!(spool::list()?.is_empty());
        assert!(tunnel::status(&c.name)?.stale);
        // A surviving port record also carries the repair responsibility.
        let _ = std::fs::remove_file(f.home.join("tunnels/remote.pid"));
    }
    assert_eq!(calls(&t), (0..3).flat_map(|_| [("remote".into(), "status".into()), ("remote".into(), "env".into())]).collect::<Vec<_>>());
    Ok(())
}

#[test]
fn empty_spool_dead_tunnel_reconnects() -> Result<()> {
    let _lock = ENV_LOCK.lock().unwrap();
    let f = Fixture::new("reconnect")?;
    let c = f.client("remote", false)?;
    f.record(&c.name, false)?;
    f.mock_ssh(true)?;
    let t = Fake::default();
    assert_eq!(dispatch::sweep_with(&t)?, 0);
    assert_eq!(f.attempts(), 1);
    let state = tunnel::status(&c.name)?;
    assert!(state.alive && !state.stale);
    assert_eq!(state.port, Some(2080));
    assert_eq!(std::fs::read_to_string(Path::new(&c.qroot).join("env"))?, "ALL_PROXY=socks5h://127.0.0.1:2080\n");
    assert_eq!(dispatch::sweep_with(&t)?, 0);
    assert_eq!(f.attempts(), 1, "a healthy tunnel must not reconnect");
    assert_eq!(calls(&t).len(), 4);
    Ok(())
}

#[test]
fn empty_spool_tunnel_respects_backoff_and_failed_probes() -> Result<()> {
    let _lock = ENV_LOCK.lock().unwrap();
    let f = Fixture::new("backoff")?;
    let c = f.client("remote", false)?;
    f.record(&c.name, true)?;
    for _ in 0..registry::CONN_FAIL_ESCALATE {
        registry::conn_fail_bump(&c.name);
    }
    let t = Fake::default();
    for idx in 1..4 {
        assert_eq!(dispatch::sweep_at(&t, idx)?, 0);
    }
    assert!(calls(&t).is_empty());
    assert_eq!(f.attempts(), 0);
    assert_eq!(dispatch::sweep_at(&t, 4)?, 0);
    assert_eq!(calls(&t), [("remote".into(), "status".into()), ("remote".into(), "env".into())]);
    assert_eq!(f.attempts(), 1);
    for t in [Fake { down: true, ..Fake::default() }, Fake { drift: true, ..Fake::default() }] {
        assert_eq!(dispatch::sweep_at(&t, 0)?, 0);
        assert_eq!(calls(&t), [("remote".into(), "status".into())]);
        assert_eq!(f.attempts(), 1, "a failed probe must not reconnect or edit env");
    }
    Ok(())
}

#[test]
fn empty_spool_unused_and_local_clients_are_noops() -> Result<()> {
    let _lock = ENV_LOCK.lock().unwrap();
    let f = Fixture::new("noop")?;
    f.client("unused", false)?;
    f.client("local", true)?;
    f.record("local", true)?;
    std::fs::write(f.home.join("tunnels/unused.log"), "old log")?;
    let t = Fake::default();
    assert_eq!(dispatch::sweep_with(&t)?, 0);
    assert!(calls(&t).is_empty());
    assert_eq!(f.attempts(), 0);
    assert!(!f.home.join("clients/localhost.toml").exists());
    Ok(())
}

#[test]
fn tunnel_workset_isolates_bad_rows_and_preserves_delivery_order() -> Result<()> {
    let _lock = ENV_LOCK.lock().unwrap();
    let f = Fixture::new("workset")?;
    for name in ["a", "b", "z"] {
        f.client(name, false)?;
        f.record(name, true)?;
    }
    for name in ["bad", "missing", "mismatch", "disabled"] {
        f.record(name, true)?;
    }
    std::fs::write(f.home.join("clients/bad.toml"), "not TOML")?;
    let mut mismatched = f.client("mismatch", false)?;
    mismatched.name = "other".into();
    std::fs::write(f.home.join("clients/mismatch.toml"), toml::to_string(&mismatched)?)?;
    let mut disabled = f.client("disabled", false)?;
    disabled.disabled = true;
    registry::save_client(&disabled)?;
    for (id, client) in [("1", "b"), ("2", "a"), ("3", "b")] {
        let e = SpoolEntry { id: id.into(), project: "p".into(), client: client.into(), queue: "default".into(), qroot: None, meta: TaskMeta { id: None, slug: format!("task-{id}"), guard: None, workdir: None, env: BTreeMap::new(), cmd: vec!["true".into()], timeout_s: None, retry: None, retry_delay_s: None, gpu_mem_mb: None, ram_mb: None, cpu_cores: None }, submitted_at: format!("2024-01-01T00:00:0{id}Z") };
        spool::stage(&e, id)?;
    }
    let t = Fake { daemon_dead: true, ..Fake::default() };
    assert_eq!(dispatch::sweep_with(&t)?, 3);
    assert_eq!(*t.deliveries.lock().unwrap(), ["1", "2", "3"]);
    let actual = calls(&t);
    for name in ["a", "b", "z"] {
        for op in ["status", "start", "env"] {
            assert_eq!(actual.iter().filter(|(c, o)| c == name && o == op).count(), 1, "{actual:?}");
        }
    }
    assert_eq!(actual.len(), 12, "bad rows must not execute anything");
    assert_eq!(f.attempts(), 3);
    assert!(spool::list()?.is_empty());
    Ok(())
}

#[test]
fn multiple_queue_deliveries_heal_one_client_tunnel() -> Result<()> {
    let _lock = ENV_LOCK.lock().unwrap();
    let f = Fixture::new("named")?;
    let mut c = f.client("remote", false)?;
    let first_root = std::mem::take(&mut c.qroot);
    let second_root = f.home.join("second-queue").display().to_string();
    for (name, qroot) in [("default", &first_root), ("second", &second_root)] {
        c.queues.insert(
            name.into(),
            serde_json::from_value(serde_json::json!({
                "qroot": qroot, "env_setup": "", "shell": "", "exit_on_idle": false,
            }))?,
        );
        std::fs::create_dir_all(qroot)?;
        std::fs::write(Path::new(qroot).join("env"), "KEEP=value\nALL_PROXY=socks5h://127.0.0.1:2080\n")?;
    }
    // Tunnel-only maintenance must also work without an execution default.
    c.default_queue = None;
    registry::save_client(&c)?;
    f.record(&c.name, true)?;
    for (id, queue, root) in [("one", "default", &first_root), ("two", "second", &second_root), ("three", "default", &first_root)] {
        let entry: SpoolEntry = serde_json::from_value(serde_json::json!({
            "id": id, "project": "", "client": c.name, "queue": queue, "qroot": root,
            "meta": {"slug": id, "guard": null, "workdir": null}, "submitted_at": id,
        }))?;
        spool::stage(&entry, id)?;
    }
    let t = Fake { daemon_dead: true, ..Fake::default() };
    assert_eq!(dispatch::sweep_with(&t)?, 3);
    assert_eq!(f.attempts(), 1, "one reconnect attempt per instance, not per queue");
    for op in ["status", "start", "env"] {
        assert_eq!(calls(&t).iter().filter(|(_, actual)| actual == op).count(), 2, "{op}");
    }
    for root in [&first_root, &second_root] {
        assert_eq!(std::fs::read_to_string(Path::new(root).join("env"))?, "KEEP=value\n");
    }
    let before = calls(&t).len();
    assert_eq!(dispatch::sweep_with(&t)?, 0);
    assert_eq!(f.attempts(), 2);
    assert_eq!(calls(&t).len() - before, 4, "one deterministic probe/start and two queue env repairs");
    Ok(())
}
