//! `client rm` local teardown: row, managed keypair, conn-fail state,
//! spool refs, and project [[bind]] rows all go together — design.md §4.1.
//! Remote decommission is best-effort and skipped for local entries.

use anyhow::Result;
use shunt::proto::task::{SpoolEntry, TaskMeta};
use shunt::server::registry::{self, Binding, ClientEntry, ProjectEntry, SshEndpoint};
use shunt::server::{client_ops, spool};
use std::collections::BTreeMap;

#[test]
fn remove_cleans_row_keys_state_spool_and_bindings() -> Result<()> {
    let home = std::env::temp_dir().join(format!("shunt-remove-{}", std::process::id()));
    std::env::set_var("SHUNT_HOME", &home);

    // Local-loopback client keeps the test box-free; ssh.key still
    // points at the managed keys/<name> path so key cleanup is checked.
    let key = registry::client_key_path("gone")?;
    std::fs::create_dir_all(key.parent().unwrap())?;
    std::fs::write(&key, "KEY")?;
    std::fs::write(key.with_extension("pub"), "ssh-ed25519 BLOB gone")?;
    let e = ClientEntry { name: "gone".into(), local: true, disabled: false, local_bin: None, timeout_s: None, ssh_timeout_s: None, shutdown_cmd: None, expire: String::new(), env_setup: String::new(), shell: String::new(), endpoint_rotates: false, ssh: SshEndpoint { host: String::new(), port: 0, user: String::new(), key: key.display().to_string(), password: String::new() }, qroot: String::new(), deploy_root: String::new(), facts: BTreeMap::new(), billing: None };
    registry::save_client(&e)?;

    spool::stage(&SpoolEntry { id: "t1".into(), project: "p".into(), client: "gone".into(), meta: TaskMeta { slug: "s".into(), guard: None, workdir: None, env: Default::default(), cmd: vec!["echo".into()], timeout_s: None }, submitted_at: "2026-01-01T00:00:00Z".into() }, "echo s\n")?;
    registry::save_project(&ProjectEntry { name: "p".into(), desc: String::new(), repo: String::new(), defaults: Default::default(), bind: vec![Binding { client: "gone".into(), workdir: "/w".into(), env: Default::default() }] })?;
    let state = home.join("state");
    std::fs::create_dir_all(&state)?;
    std::fs::write(state.join("gone.fail"), "2\n")?;

    client_ops::remove("gone", false)?;

    assert!(registry::load_client("gone").is_err());
    assert!(!key.exists() && !key.with_extension("pub").exists());
    assert!(!state.join("gone.fail").exists());
    assert!(spool::list()?.is_empty());
    assert!(registry::load_project("p")?.bind.is_empty());

    // Guards: localhost refused, missing row errors.
    assert!(client_ops::remove("localhost", false).is_err());
    assert!(client_ops::remove("gone", false).is_err());

    std::fs::remove_dir_all(&home).ok();
    Ok(())
}
