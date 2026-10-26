use super::push;
use crate::proto::task::{SpoolEntry, TaskMeta};
use crate::server::registry::{ClientEntry, ResolvedQueue, SshEndpoint};
use crate::server::transport::Transport;
use anyhow::Result;
use std::path::Path;
use std::sync::Mutex;

#[derive(Default)]
struct Capture {
    argv: Mutex<Vec<String>>,
    stdin: Mutex<Option<String>>,
}

impl Transport for Capture {
    fn exec(&self, _: &ClientEntry, argv: &[String], stdin: Option<&str>) -> Result<String> {
        *self.argv.lock().unwrap() = argv.to_vec();
        *self.stdin.lock().unwrap() = stdin.map(str::to_string);
        Ok(r#"{"protocol":1,"ok":true,"name":"0100_train"}"#.into())
    }

    fn copy_to(&self, _: &ClientEntry, _: &Path, _: &str) -> Result<()> {
        unreachable!("enqueue does not copy files")
    }
}

/// An expired row is never retried (design.md §9): the sweep leaves the
/// spool entry staged and never touches the transport, so a box the
/// vendor has already reclaimed costs nothing per poll.
struct NoTouch;

impl Transport for NoTouch {
    fn exec(&self, _: &ClientEntry, _: &[String], _: Option<&str>) -> Result<String> {
        unreachable!("an expired row must never be probed")
    }

    fn copy_to(&self, _: &ClientEntry, _: &Path, _: &str) -> Result<()> {
        unreachable!("an expired row must never receive a task")
    }
}

#[test]
fn expired_client_is_never_dispatched() {
    let _g = crate::server::registry::tests::HOME_LOCK.lock().unwrap();
    let tmp = std::env::temp_dir().join(format!("shunt-test-expired-sweep-{}", std::process::id()));
    std::env::set_var("SHUNT_HOME", &tmp);
    std::fs::create_dir_all(tmp.join("clients")).unwrap();
    std::fs::write(tmp.join("clients/box.toml"), "name = \"box\"\nqroot = \"/queue\"\ndeploy_root = \"/root\"\nexpire = \"2020-01-01T00:00:00Z\"\n[ssh]\nhost = \"127.0.0.1\"\nport = 1\nuser = \"u\"\n").unwrap();
    let entry = SpoolEntry { id: "task-1".into(), project: String::new(), client: "box".into(), queue: "default".into(), qroot: Some("/queue".into()), hold: None, hold_on_box: None, meta: TaskMeta { id: Some("task-1".into()), slug: "train-task1".into(), guard: None, workdir: None, env: Default::default(), cmd: vec![], timeout_s: None, retry: None, retry_delay_s: None, gpu_mem_mb: None, ram_mb: None, cpu_cores: None }, submitted_at: "now".into() };
    crate::server::spool::stage(&entry, "#!/bin/sh\ntrue\n").unwrap();

    assert_eq!(super::sweep_with(&NoTouch).unwrap(), 0);
    assert_eq!(crate::server::spool::list().unwrap().len(), 1, "the entry stays staged for a renewal");
    std::fs::remove_dir_all(&tmp).ok();
}

#[test]
fn dispatcher_forwards_box_hold_only_when_requested() {
    let transport = Capture::default();
    let queue = ResolvedQueue { name: "default".into(), client: ClientEntry { name: "box".into(), local: false, disabled: false, local_bin: None, bin_path: None, timeout_s: None, ssh_timeout_s: None, pre_connect: String::new(), shutdown_cmd: None, expire: String::new(), env_setup: String::new(), shell: String::new(), endpoint_rotates: false, shared: false, exit_on_idle: false, ssh: SshEndpoint::default(), qroot: "/queue".into(), queues: Default::default(), default_queue: None, deploy_root: "/".into(), facts: Default::default(), billing: None }, workers: None };
    let entry = SpoolEntry { id: "task-1".into(), project: String::new(), client: "box".into(), queue: "default".into(), qroot: Some("/queue".into()), hold: None, hold_on_box: Some("await review".into()), meta: TaskMeta { id: Some("task-1".into()), slug: "train-task1".into(), guard: None, workdir: None, env: Default::default(), cmd: vec![], timeout_s: None, retry: None, retry_delay_s: None, gpu_mem_mb: None, ram_mb: None, cpu_cores: None }, submitted_at: "now".into() };

    assert_eq!(push(&transport, &queue, &entry, "script\n").unwrap(), ("0100_train".into(), false));
    let argv = transport.argv.lock().unwrap();
    assert!(argv.iter().any(|arg| arg == "--hold=await review"), "hold flag missing from wire argv: {argv:?}");
    assert_eq!(transport.stdin.lock().unwrap().as_deref(), Some("script\n"));
}
