//! Registry-level behaviors that need process-local SHUNT_HOME:
//! key generation on registration, localhost materialization, and the
//! disabled flag's effect on listings.

use anyhow::Result;
use shunt::server::client_ops::ensure_key;
use shunt::server::registry::{self, ClientEntry, SshEndpoint};
use std::collections::BTreeMap;

fn ssh_entry(name: &str) -> ClientEntry {
    ClientEntry { name: name.into(), local: false, disabled: false, local_bin: None, timeout_s: None, ssh_timeout_s: None, shutdown_cmd: None, expire: String::new(), ssh: SshEndpoint { host: "example.invalid".into(), port: 22, user: "root".into(), key: String::new(), password: String::new() }, qroot: "/q".into(), deploy_root: "/q".into(), facts: BTreeMap::new(), billing: None }
}

#[test]
fn ensure_key_generates_reuses_and_errors() -> Result<()> {
    let home = std::env::temp_dir().join(format!("shunt-keytest-{}", std::process::id()));
    std::env::set_var("SHUNT_HOME", &home);

    let mut e = ssh_entry("box-k");
    ensure_key(&mut e)?;
    let k = registry::client_key_path("box-k")?;
    assert!(k.exists() && k.with_extension("pub").exists());
    assert_eq!(e.ssh.key, k.display().to_string());

    ensure_key(&mut e)?; // idempotent: same path back
    assert_eq!(e.ssh.key, k.display().to_string());

    // A user-set path that does not exist is surfaced, not clobbered.
    e.ssh.key = "/missing/key".into();
    assert!(ensure_key(&mut e).is_err());

    std::fs::remove_dir_all(&home).ok();
    Ok(())
}
