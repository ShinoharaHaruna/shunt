//! `client rename` moves the whole identity: row file + name field,
//! managed keypair, mux socket, conn-fail streak, tunnel state,
//! spool entry refs, and project [[bind]] refs — design.md §4.1.

use anyhow::Result;
use shunt::proto::task::{SpoolEntry, TaskMeta};
use shunt::server::registry::{self, Binding, ClientEntry, ProjectEntry, SshEndpoint};
use shunt::server::{client_ops, spool};
use std::collections::BTreeMap;

fn entry(name: &str) -> ClientEntry {
    ClientEntry { name: name.into(), local: false, disabled: false, local_bin: None, timeout_s: None, ssh_timeout_s: None, shutdown_cmd: None, expire: String::new(), env_setup: String::new(), shell: String::new(), endpoint_rotates: false, ssh: SshEndpoint { host: "example.invalid".into(), port: 22, user: "root".into(), key: String::new(), password: String::new() }, qroot: "/q".into(), deploy_root: "/q".into(), facts: BTreeMap::new(), billing: None }
}

#[test]
fn rename_moves_row_keys_state_and_refs() -> Result<()> {
    let home = std::env::temp_dir().join(format!("shunt-rename-{}", std::process::id()));
    std::env::set_var("SHUNT_HOME", &home);

    // Row + managed keypair.
    let mut e = entry("old-box");
    client_ops::ensure_key(&mut e)?;
    registry::save_client(&e)?;
    // A staged submission + a project binding referencing old-box.
    spool::stage(&SpoolEntry { id: "t1".into(), project: "p".into(), client: "old-box".into(), meta: TaskMeta { slug: "s".into(), guard: None, workdir: None, env: Default::default(), cmd: vec!["echo".into()] }, submitted_at: "2026-01-01T00:00:00Z".into() }, "echo s\n")?;
    registry::save_project(&ProjectEntry { name: "p".into(), bind: vec![Binding { client: "old-box".into(), workdir: "/w".into(), env: Default::default() }] })?;
    // Conn-fail streak file.
    let state = home.join("state");
    std::fs::create_dir_all(&state)?;
    std::fs::write(state.join("old-box.fail"), "3\n")?;

    client_ops::rename("old-box", "new-box")?;

    let moved = registry::load_client("new-box")?;
    assert_eq!(moved.name, "new-box");
    assert!(moved.ssh.key.ends_with("keys/new-box"), "{}", moved.ssh.key);
    assert!(home.join("keys/new-box").exists() && home.join("keys/new-box.pub").exists());
    assert!(!home.join("clients/old-box.toml").exists());
    assert!(registry::load_client("old-box").is_err());
    assert_eq!(registry::conn_fail_read("new-box"), 3);
    assert_eq!(registry::conn_fail_read("old-box"), 0);
    assert_eq!(spool::list()?[0].client, "new-box");
    assert_eq!(registry::load_project("p")?.bind[0].client, "new-box");

    // Guards: no clobber, no reserved name.
    assert!(client_ops::rename("new-box", "new-box-2").is_ok());
    assert!(client_ops::rename("new-box-2", "localhost").is_err());
    assert!(client_ops::rename("localhost", "x").is_err());

    std::fs::remove_dir_all(&home).ok();
    Ok(())
}
