//! Dispatcher viability gates: inactive clients are reported without
//! transport work, quarantines stop dispatch, and client updates recover them.

use anyhow::Result;
use shunt::proto::task::{SpoolEntry, TaskMeta};
use shunt::server::dispatch;
use shunt::server::registry::{self, ClientEntry, QueueEntry, SshEndpoint};
use shunt::server::spool;
use shunt::server::transport::Transport;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

static HOME_LOCK: Mutex<()> = Mutex::new(());

fn home(tag: &str) -> Result<PathBuf> {
    let path = std::env::temp_dir().join(format!("shunt-viability-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&path)?;
    std::env::set_var("SHUNT_HOME", &path);
    Ok(path)
}

fn client(name: &str) -> ClientEntry {
    ClientEntry { name: name.into(), local: false, disabled: false, local_bin: None, bin_path: None, timeout_s: None, ssh_timeout_s: None, pre_connect: String::new(), shutdown_cmd: None, expire: String::new(), env_setup: String::new(), shell: String::new(), endpoint_rotates: false, shared: false, exit_on_idle: false, ssh: SshEndpoint { host: "127.0.0.1".into(), port: 22, user: "root".into(), key: String::new(), password: String::new(), options: Vec::new() }, qroot: "/root/queue".into(), deploy_root: "/root".into(), facts: BTreeMap::new(), billing: None, queues: BTreeMap::<String, QueueEntry>::new(), default_queue: None }
}

fn entry(id: &str, client: &str) -> SpoolEntry {
    SpoolEntry { id: id.into(), project: "p".into(), client: client.into(), queue: "default".into(), qroot: None, hold: None, hold_on_box: None, meta: TaskMeta { id: None, slug: format!("task-{id}"), guard: None, workdir: None, env: BTreeMap::new(), cmd: vec!["echo".into(), "hello".into()], timeout_s: None, retry: None, retry_delay_s: None, gpu_mem_mb: None, ram_mb: None, cpu_cores: None }, submitted_at: "2024-01-01T00:00:00Z".into() }
}

fn status_for(name: &str, force: bool) -> Result<serde_json::Value> {
    let status = shunt::server::status::probe_all(force)?;
    let row = status.into_iter().find(|row| row.name == name).expect("client appears in status");
    Ok(serde_json::to_value(row.status)?)
}

fn show(name: &str, home: &Path) -> Result<String> {
    let output = Command::new(env!("CARGO_BIN_EXE_shunt")).args(["client", "show", name]).env("SHUNT_HOME", home).output()?;
    anyhow::ensure!(output.status.success(), "client show failed: {}", String::from_utf8_lossy(&output.stderr));
    Ok(String::from_utf8(output.stdout)?)
}

struct NoTouchTransport(AtomicUsize);

impl Transport for NoTouchTransport {
    fn exec(&self, _client: &ClientEntry, _argv: &[String], _stdin: Option<&str>) -> Result<String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        anyhow::bail!("inactive client must not be contacted");
    }

    fn copy_to(&self, _client: &ClientEntry, _local: &Path, _remote: &str) -> Result<()> {
        self.0.fetch_add(1, Ordering::SeqCst);
        anyhow::bail!("inactive client must not be contacted");
    }
}

#[test]
fn disabled_and_expired_clients_are_silent_and_visible() -> Result<()> {
    let _lock = HOME_LOCK.lock().unwrap();
    let home = home("inactive")?;
    let marker = home.join("transport-called");
    let script = home.join("local-transport");
    std::fs::write(&script, "#!/bin/sh\nprintf called >> \"$SHUNT_TEST_TRANSPORT\"\n")?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))?;

    let mut disabled = client("parked");
    disabled.local = true;
    disabled.disabled = true;
    disabled.local_bin = Some(script.display().to_string());
    registry::save_client(&disabled)?;

    let mut expired = client("expired");
    expired.local = true;
    expired.local_bin = Some(script.display().to_string());
    expired.expire = "2020-01-01T00:00:00Z".into();
    registry::save_client(&expired)?;

    let mut localhost = client("localhost");
    localhost.local = true;
    localhost.disabled = true;
    registry::save_client(&localhost)?;

    spool::stage(&entry("disabled-task", "parked"), "echo parked\n")?;
    spool::stage(&entry("expired-task", "expired"), "echo expired\n")?;

    for (name, reason) in [("parked", "disabled"), ("expired", "expired")] {
        let status = status_for(name, false)?;
        assert_eq!(status["inactive"]["reason"], reason);
        assert_eq!(status["skip"]["reason"], reason);
        assert!(show(name, &home)?.contains(&format!("# inactive: {reason}")));
    }
    let selected = Command::new(env!("CARGO_BIN_EXE_shunt")).args(["status", "-c", "parked", "--json"]).env("SHUNT_HOME", &home).output()?;
    anyhow::ensure!(selected.status.success(), "status -c disabled failed: {}", String::from_utf8_lossy(&selected.stderr));
    let selected: serde_json::Value = serde_json::from_slice(&selected.stdout)?;
    assert_eq!(selected["inactive"]["reason"], "disabled");

    let output = Command::new(env!("CARGO_BIN_EXE_shunt")).args(["dispatch", "--once"]).env("SHUNT_HOME", &home).env("SHUNT_TEST_TRANSPORT", &marker).output()?;
    anyhow::ensure!(output.status.success(), "dispatch failed: {}", String::from_utf8_lossy(&output.stderr));
    assert!(output.stderr.is_empty(), "inactive rows must not log per sweep: {}", String::from_utf8_lossy(&output.stderr));
    assert!(!marker.exists(), "dispatcher performed transport work for an inactive client");
    assert_eq!(spool::list()?.len(), 2, "inactive work remains staged");

    std::env::remove_var("SHUNT_HOME");
    std::fs::remove_dir_all(home).ok();
    Ok(())
}

#[test]
fn quarantine_is_reported_skipped_and_cleared_by_client_update() -> Result<()> {
    let _lock = HOME_LOCK.lock().unwrap();
    let home = home("quarantine")?;
    let mut remote = client("box-a");
    remote.expire = "2999-01-01T00:00:00Z".into();
    registry::save_client(&remote)?;
    let mut localhost = client("localhost");
    localhost.local = true;
    localhost.disabled = true;
    registry::save_client(&localhost)?;

    for _ in 0..registry::CONN_FAIL_QUARANTINE {
        registry::conn_fail_bump("box-a");
    }
    spool::stage(&entry("quarantined-task", "box-a"), "echo waiting\n")?;

    let transport = NoTouchTransport(AtomicUsize::new(0));
    assert_eq!(dispatch::sweep_at(&transport, 40)?, 0);
    assert_eq!(transport.0.load(Ordering::SeqCst), 0);
    let output = Command::new(env!("CARGO_BIN_EXE_shunt")).args(["dispatch", "--once"]).env("SHUNT_HOME", &home).output()?;
    anyhow::ensure!(output.status.success(), "dispatch failed: {}", String::from_utf8_lossy(&output.stderr));
    assert!(output.stderr.is_empty(), "quarantined rows must not log per sweep: {}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(spool::list()?.len(), 1);

    let status = status_for("box-a", false)?;
    assert_eq!(status["inactive"]["reason"], "quarantined");
    assert_eq!(status["inactive"]["consecutive_failures"], registry::CONN_FAIL_QUARANTINE);
    assert_eq!(status["skip"]["reason"], "quarantined");
    assert!(show("box-a", &home)?.contains("# inactive: quarantined (9 consecutive failures)"));

    shunt::server::client_ops::update("box-a", shunt::server::client_ops::EndpointUpdate { host: None, port: None, user: None, key: None, password: None, pre_connect: None, endpoint_rotates: None, options: Some(vec!["ConnectTimeout=10".into()]) })?;
    let updated = registry::load_client("box-a")?;
    assert_eq!(registry::conn_fail_streak(&updated), 0);
    assert!(!show("box-a", &home)?.contains("# inactive:"));

    std::env::remove_var("SHUNT_HOME");
    std::fs::remove_dir_all(home).ok();
    Ok(())
}
