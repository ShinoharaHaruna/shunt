//! Unit tests plus `HOME_LOCK`, which serializes every test in the
//! crate that mutates the process-global `SHUNT_HOME`.

use super::*;
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;

// SHUNT_HOME is process-global and set_var races cross module
// boundaries — every test in the crate that touches it must hold
// this one lock.
pub(crate) static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn client_roundtrip() {
    let _g = HOME_LOCK.lock().unwrap();
    let tmp = std::env::temp_dir().join(format!("shunt-test-{}", std::process::id()));
    std::env::set_var("SHUNT_HOME", &tmp);
    let entry = ClientEntry { name: "box-1".into(), local: false, disabled: false, local_bin: None, timeout_s: None, ssh_timeout_s: None, shutdown_cmd: None, expire: String::new(), env_setup: String::new(), shell: String::new(), endpoint_rotates: false, ssh: SshEndpoint { host: "example.invalid".into(), port: 22, user: "root".into(), key: "/path/to/key".into(), password: String::new() }, qroot: "/root/autodl-tmp/queue".into(), deploy_root: "/root/autodl-tmp".into(), facts: BTreeMap::new(), billing: Some(Billing { hourly: 2.6, currency: "cny".into() }) };
    save_client(&entry).unwrap();
    let back = load_client("box-1").unwrap();
    assert_eq!(back.ssh.port, 22);
    assert_eq!(list_names("clients").unwrap(), vec!["box-1"]);

    // localhost is a peer: first access materializes its row file
    // with the defaults, and it lists like any other client.
    let lo = load_client("localhost").unwrap();
    assert!(lo.local);
    assert_eq!(lo.qroot, tmp.join("queue").display().to_string());
    assert!(tmp.join("clients/localhost.toml").exists());
    let lo2 = load_client("localhost").unwrap(); // second load: from disk
    assert_eq!(lo2.qroot, lo.qroot);
    assert_eq!(list_clients().unwrap(), vec!["box-1", "localhost"]);

    // disabled=true opts out of being a client: load_client rejects
    // and list_clients skips, but the row stays on disk.
    save_client(&ClientEntry { name: "gone".into(), disabled: true, ..entry.clone() }).unwrap();
    assert!(load_client("gone").is_err());
    let mut lo_disabled = lo.clone();
    lo_disabled.disabled = true;
    save_client(&lo_disabled).unwrap();
    assert!(load_client("localhost").is_err());
    assert_eq!(list_clients().unwrap(), vec!["box-1"]);

    // rm = reset to factory: delete the row, next load regenerates
    // the defaults (also how a disabled localhost comes back).
    remove("clients", "localhost").unwrap();
    let lo3 = load_client("localhost").unwrap();
    assert!(lo3.local && !lo3.disabled);
    assert!(tmp.join("clients/localhost.toml").exists());
    std::fs::remove_dir_all(&tmp).ok();
}

/// Registry privacy: client rows can carry ssh.password
/// plaintext, so dirs are 0700 and files 0600 regardless of
/// umask — including re-tightening a pre-existing loose file.
#[test]
fn registry_files_are_private() {
    let _g = HOME_LOCK.lock().unwrap();
    let tmp = std::env::temp_dir().join(format!("shunt-test-mode-{}", std::process::id()));
    std::env::set_var("SHUNT_HOME", &tmp);
    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;

    // First access materializes the localhost row's defaults.
    let entry = load_client("localhost").unwrap();
    let row = save_client(&entry).unwrap();
    assert_eq!(mode(&tmp), 0o700, "shunt_home root");
    assert_eq!(mode(&tmp.join("clients")), 0o700, "clients/");
    assert_eq!(mode(&row), 0o600, "client row");

    // A pre-existing loose row is re-tightened on the next save.
    std::fs::set_permissions(&row, std::fs::Permissions::from_mode(0o644)).unwrap();
    save_client(&entry).unwrap();
    assert_eq!(mode(&row), 0o600, "re-saved row");

    // state/ (conn-fail streaks) is private too.
    conn_fail_bump("box");
    assert_eq!(mode(&tmp.join("state")), 0o700, "state/");
    assert_eq!(mode(&tmp.join("state/box.fail")), 0o600, "fail file");
    std::fs::remove_dir_all(&tmp).ok();
}
