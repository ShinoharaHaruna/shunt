//! Registry-level behaviors that need process-local SHUNT_HOME:
//! key generation on registration, localhost materialization, and the
//! disabled flag's effect on listings.

use anyhow::Result;
use shunt::server::client_ops::{self, ensure_key};
use shunt::server::registry::{self, ClientEntry, SshEndpoint};
use std::collections::BTreeMap;

// SHUNT_HOME is process-global; tests in this binary run on
// parallel threads, so every test that sets it must hold this lock
// for its whole body or the two paths interleave.
static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn ssh_entry(name: &str) -> ClientEntry {
    ClientEntry { name: name.into(), local: false, disabled: false, local_bin: None, timeout_s: None, ssh_timeout_s: None, shutdown_cmd: None, expire: String::new(), env_setup: String::new(), shell: String::new(), endpoint_rotates: false, ssh: SshEndpoint { host: "example.invalid".into(), port: 22, user: "root".into(), key: String::new(), password: String::new() }, qroot: "/q".into(), deploy_root: "/q".into(), facts: BTreeMap::new(), billing: None }
}

#[test]
fn ensure_key_generates_reuses_and_errors() -> Result<()> {
    let _g = HOME_LOCK.lock().unwrap();
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

#[test]
fn client_update_rewrites_endpoint_and_clears_state() -> Result<()> {
    let _g = HOME_LOCK.lock().unwrap();
    let home = std::env::temp_dir().join(format!("shunt-uptest-{}", std::process::id()));
    std::env::set_var("SHUNT_HOME", &home);

    let mut e = ssh_entry("box-u");
    e.billing = Some(registry::Billing { hourly: 2.6, currency: "cny".into() });
    registry::save_client(&e)?;
    // A hot conn-fail streak belongs to the OLD endpoint.
    for _ in 0..3 {
        registry::conn_fail_bump("box-u");
    }
    assert_eq!(registry::conn_fail_read("box-u"), 3);

    client_ops::update("box-u", client_ops::EndpointUpdate { host: Some("new.example.invalid".into()), port: Some(4022), user: None, password: Some("pw".into()), endpoint_rotates: Some(true) })?;

    let back = registry::load_client("box-u")?;
    assert_eq!(back.ssh.host, "new.example.invalid");
    assert_eq!(back.ssh.port, 4022);
    assert_eq!(back.ssh.user, "root"); // untouched fields stay
    assert_eq!(back.ssh.password, "pw");
    assert!(back.endpoint_rotates);
    assert_eq!(back.billing.unwrap().hourly, 2.6);
    assert_eq!(registry::conn_fail_read("box-u"), 0);

    // No flags is a usage error, not a silent no-op; port 0 is not
    // an endpoint.
    let none = client_ops::EndpointUpdate { host: None, port: None, user: None, password: None, endpoint_rotates: None };
    assert!(client_ops::update("box-u", none).is_err());
    let p0 = client_ops::EndpointUpdate { host: None, port: Some(0), user: None, password: None, endpoint_rotates: None };
    assert!(client_ops::update("box-u", p0).is_err());
    assert!(registry::load_client("box-u")?.ssh.port == 4022);

    // Tri-state endpoint_rotates: Some(false) clears a mislabeled
    // row and alone counts as a real update; None leaves it be.
    client_ops::update("box-u", client_ops::EndpointUpdate { host: None, port: None, user: None, password: None, endpoint_rotates: Some(false) })?;
    assert!(!registry::load_client("box-u")?.endpoint_rotates);

    std::fs::remove_dir_all(&home).ok();
    Ok(())
}
