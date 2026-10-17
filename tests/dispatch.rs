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
    calls: Mutex<Vec<String>>,
}

impl Transport for FakeTransport {
    fn exec(&self, client: &ClientEntry, argv: &[String], stdin: Option<&str>) -> Result<String> {
        self.calls
            .lock()
            .unwrap()
            .push(argv.get(1).cloned().unwrap_or_default());
        if self.down {
            anyhow::bail!("ssh connect timeout");
        }
        match argv.get(1).map(String::as_str) {
            Some("status") => Ok(r#"{"protocol":1,"daemon":{"alive":true,"pid":1234},
                "workers":{"value":3,"source":"default"},"running":[],"pending":[],
                "done_recent":[],"failed_recent":[],"stop":false,
                "shutdown":{"armed":false,"grace_s":60}}"#
                .into()),
            Some("enqueue") => {
                if self.fail_enqueue {
                    anyhow::bail!("remote rc=1: disk full");
                }
                let body = stdin.unwrap_or("");
                let name = "0100_task.sh";
                std::fs::write(self.pending.join(name), body)?;
                Ok(format!(
                    r#"{{"protocol":1,"ok":true,"name":"{name}","client":"{}"}}"#,
                    client.name
                ))
            }
            _ => Ok(r#"{"protocol":1,"ok":true}"#.into()),
        }
    }

    fn copy_to(&self, _client: &ClientEntry, _local: &Path, _remote: &str) -> Result<()> {
        Ok(())
    }
}

fn fixture() -> Result<(PathBuf, ClientEntry)> {
    let home = std::env::temp_dir().join(format!("shunt-itest-{}", std::process::id()));
    std::fs::create_dir_all(&home)?;
    std::env::set_var("SHUNT_HOME", &home);
    let client = ClientEntry {
        name: "fake-a".into(),
        local: false,
        disabled: false,
        local_bin: None,
        timeout_s: None,
        ssh_timeout_s: None,
        shutdown_cmd: None,
        ssh: SshEndpoint {
            host: "127.0.0.1".into(),
            port: 22,
            user: "root".into(),
            key: String::new(),
        },
        qroot: "/root/queue".into(),
        deploy_root: "/root".into(),
        facts: BTreeMap::new(),
        billing: None,
    };
    registry::save_client(&client)?;
    Ok((home, client))
}

fn entry(id: &str, slug: &str) -> SpoolEntry {
    SpoolEntry {
        id: id.into(),
        project: "p".into(),
        client: "fake-a".into(),
        meta: TaskMeta {
            slug: slug.into(),
            guard: None,
            workdir: None,
            env: BTreeMap::new(),
            cmd: vec!["echo".into(), "hi".into()],
        },
        submitted_at: "2024-01-01T00:00:00Z".into(),
    }
}

#[test]
fn dispatch_ack_deletes_failure_retains() -> Result<()> {
    let (home, _client) = fixture()?;
    let pending = home.join("pending");
    std::fs::create_dir_all(&pending)?;

    // Clean ack: pushed and removed from spool.
    spool::stage(&entry("t1", "a-t1"), "echo t1\n")?;
    let t = FakeTransport {
        pending: pending.clone(),
        fail_enqueue: false,
        down: false,
        calls: Mutex::new(Vec::new()),
    };
    assert_eq!(dispatch::sweep_with(&t)?, 1);
    assert!(spool::list()?.is_empty());
    assert_eq!(
        std::fs::read_to_string(pending.join("0100_task.sh"))?,
        "echo t1\n"
    );

    // Remote enqueue error: entry stays for the next poll.
    spool::stage(&entry("t2", "a-t2"), "echo t2\n")?;
    let t_fail = FakeTransport {
        pending: pending.clone(),
        fail_enqueue: true,
        down: false,
        calls: Mutex::new(Vec::new()),
    };
    assert_eq!(dispatch::sweep_with(&t_fail)?, 0);
    assert_eq!(spool::list()?.len(), 1);

    // Unreachable box: entry stays, no error.
    spool::stage(&entry("t3", "a-t3"), "echo t3\n")?;
    let t_down = FakeTransport {
        pending: pending.clone(),
        fail_enqueue: false,
        down: true,
        calls: Mutex::new(Vec::new()),
    };
    assert_eq!(dispatch::sweep_with(&t_down)?, 0);
    assert_eq!(spool::list()?.len(), 2);

    std::fs::remove_dir_all(&home).ok();
    Ok(())
}
