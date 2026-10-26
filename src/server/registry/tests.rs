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
    let entry = ClientEntry { name: "box-1".into(), local: false, disabled: false, local_bin: None, bin_path: None, timeout_s: None, ssh_timeout_s: None, pre_connect: String::new(), shutdown_cmd: None, expire: String::new(), env_setup: String::new(), shell: String::new(), endpoint_rotates: false, shared: false, exit_on_idle: false, ssh: SshEndpoint { host: "example.invalid".into(), port: 22, user: "root".into(), key: "/path/to/key".into(), password: String::new(), options: vec!["ProxyJump=jump.example".into()] }, qroot: "/root/autodl-tmp/queue".into(), queues: BTreeMap::new(), default_queue: None, deploy_root: "/root/autodl-tmp".into(), facts: BTreeMap::new(), billing: Some(Billing { hourly: 2.6, currency: "cny".into() }) };
    save_client(&entry).unwrap();
    let back = load_client("box-1").unwrap();
    assert_eq!(back.ssh.port, 22);
    assert_eq!(back.ssh.options, vec!["ProxyJump=jump.example"]);
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

#[test]
fn ssh_options_default_when_absent_and_omit_when_empty() {
    let old: SshEndpoint = toml::from_str(r#"host = 'box.example'"#).unwrap();
    assert!(old.options.is_empty());
    let serialized = toml::to_string(&SshEndpoint::default()).unwrap();
    assert!(!serialized.contains("options"), "{serialized}");
}

/// The `.fail` sidecar records the streak and the epoch of the newest
/// failure (design.md §4.1 probe penalty). A file in the pre-timestamp
/// single-line format still yields its count with no clock — a
/// downgraded or hand-written streak never loses the count.
#[test]
fn conn_fail_sidecar_keeps_count_and_epoch() {
    let _g = HOME_LOCK.lock().unwrap();
    let tmp = std::env::temp_dir().join(format!("shunt-test-fail-{}", std::process::id()));
    std::env::set_var("SHUNT_HOME", &tmp);
    assert_eq!(conn_fail_read("box"), 0);
    assert_eq!(conn_fail_at("box"), None);

    assert_eq!(conn_fail_bump("box"), 1);
    assert_eq!(conn_fail_bump("box"), 2);
    let at = conn_fail_at("box").expect("a bump stamps the clock");
    let now = crate::proto::time::unix_now() as i64;
    assert!((now - at).abs() <= 5, "stamped {at} vs now {now}");

    // Pre-upgrade format: count only, no epoch.
    std::fs::write(tmp.join("state/box.fail"), "7\n").unwrap();
    assert_eq!(conn_fail_read("box"), 7);
    assert_eq!(conn_fail_at("box"), None);

    conn_fail_reset("box");
    assert_eq!(conn_fail_read("box"), 0);
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

/// server.toml's default_client tracks renames/removes (design.md
/// §4.1): rename rewrites it, rm drops the key, comments and other
/// content survive, and non-matching/broken files are left alone.
#[test]
fn retarget_default_client_tracks_identity() {
    let _g = HOME_LOCK.lock().unwrap();
    let tmp = std::env::temp_dir().join(format!("shunt-test-defcli-{}", std::process::id()));
    std::env::set_var("SHUNT_HOME", &tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let path = tmp.join("server.toml");

    // Rename: key retargets, comments and other keys preserved.
    std::fs::write(&path, "# my config\npoll_interval_s = 30\ndefault_client = \"gpu-a\" # main box\n[extra]\nx = 1\n").unwrap();
    assert!(retarget_default_client("gpu-a", Some("gpu-b")).unwrap());
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(text, "# my config\npoll_interval_s = 30\ndefault_client = \"gpu-b\"\n[extra]\nx = 1\n");
    assert_eq!(load_server_config().unwrap().default_client.as_deref(), Some("gpu-b"));

    // Non-matching default is untouched.
    assert!(!retarget_default_client("other", Some("gpu-c")).unwrap());
    assert_eq!(load_server_config().unwrap().default_client.as_deref(), Some("gpu-b"));

    // Rm: key line drops, required_client's "no default" path applies.
    assert!(retarget_default_client("gpu-b", None).unwrap());
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(text, "# my config\npoll_interval_s = 30\n[extra]\nx = 1\n");
    assert_eq!(load_server_config().unwrap().default_client, None);

    // Missing file and broken TOML: false / Err, never a panic.
    std::fs::remove_file(&path).unwrap();
    assert!(!retarget_default_client("gpu-b", None).unwrap());
    std::fs::write(&path, "this is = = not toml\n").unwrap();
    assert!(retarget_default_client("gpu-b", None).is_err());

    std::env::remove_var("SHUNT_HOME");
    std::fs::remove_dir_all(&tmp).ok();
}

/// Registry names become filenames under $SHUNT_HOME — the charset
/// gate is what keeps `client add ../x` or `project add -rf` from
/// escaping their dirs or parsing as a flag downstream.
#[test]
fn registry_name_charset() {
    for ok in ["box-1", "gpu.a", "a_b", "localhost", "x9"] {
        assert!(valid_name(ok).is_ok(), "{ok:?} should be valid");
    }
    for bad in ["", ".", "..", "../x", "a/b", "a b", "-rf", "a$()", "a;b"] {
        assert!(valid_name(bad).is_err(), "{bad:?} must be rejected");
    }
    // The io layer enforces it too: load/save/remove reject bad names.
    let _g = HOME_LOCK.lock().unwrap();
    let tmp = std::env::temp_dir().join(format!("shunt-test-name-{}", std::process::id()));
    std::env::set_var("SHUNT_HOME", &tmp);
    assert!(load_client("../x").is_err());
    assert!(remove("clients", "../../victim").is_err());
    std::fs::remove_dir_all(&tmp).ok();
}

/// A row's inner `name` — not the filename — is what joins into
/// cm/, state/, known_hosts/, tunnels/ paths and selects the
/// localhost loopback transport in transport_for. A file whose
/// `name` disagrees with its stem (or isn't a valid name at all)
/// fails to load rather than write outside $SHUNT_HOME.
#[test]
fn row_name_must_match_filename() {
    let _g = HOME_LOCK.lock().unwrap();
    let tmp = std::env::temp_dir().join(format!("shunt-test-inner-{}", std::process::id()));
    std::env::set_var("SHUNT_HOME", &tmp);
    let clients = tmp.join("clients");
    std::fs::create_dir_all(&clients).unwrap();
    let row = |name: &str| format!("name = {name:?}\nqroot = \"/q\"\ndeploy_root = \"/d\"\n");

    // Forged inner name: filename is safe, name is a traversal.
    std::fs::write(clients.join("a.toml"), row("../../x")).unwrap();
    assert!(load_client("a").is_err());

    // Loopback spoof: any file claiming name="localhost" that is
    // not localhost.toml would silently flip to local transport.
    std::fs::write(clients.join("b.toml"), row("localhost")).unwrap();
    assert!(load_client("b").is_err());

    // Plain mismatch (a rename done on disk but not in the field).
    std::fs::write(clients.join("c.toml"), row("other")).unwrap();
    let err = load_client("c").unwrap_err().to_string();
    assert!(err.contains("other"), "error names the discrepancy: {err}");

    // Projects are held to the same rule.
    let projects = tmp.join("projects");
    std::fs::create_dir_all(&projects).unwrap();
    std::fs::write(projects.join("p.toml"), "name = \"q\"\n").unwrap();
    assert!(load_project("p").is_err());

    // Matching rows still load.
    std::fs::write(clients.join("ok.toml"), row("ok")).unwrap();
    assert!(load_client("ok").is_ok());
    std::fs::remove_dir_all(&tmp).ok();
}

/// An empty qroot is an unusable row: `shunt_bin` would render as
/// `/bin/shunt` (stage-then-mv into SYSTEM /bin), wire ops would
/// pass `--root ""`, and a loopback row would drop queue dirs in
/// the local cwd. load_client rejects it; the row still parses, so
/// list_clients keeps it and `status --all` degrades to an error
/// row instead of dropping or killing the aggregate.
#[test]
fn empty_qroot_row_is_unusable() {
    let _g = HOME_LOCK.lock().unwrap();
    let tmp = std::env::temp_dir().join(format!("shunt-test-qroot-{}", std::process::id()));
    std::env::set_var("SHUNT_HOME", &tmp);
    let entry = ClientEntry { name: "noq".into(), local: false, disabled: false, local_bin: None, bin_path: None, timeout_s: None, ssh_timeout_s: None, pre_connect: String::new(), shutdown_cmd: None, expire: String::new(), env_setup: String::new(), shell: String::new(), endpoint_rotates: false, shared: false, exit_on_idle: false, ssh: SshEndpoint::default(), qroot: String::new(), queues: BTreeMap::new(), default_queue: None, deploy_root: String::new(), facts: BTreeMap::new(), billing: None };
    save_client(&entry).unwrap();

    let err = load_client("noq").unwrap_err().to_string();
    assert!(err.contains("qroot is empty"), "{err}");

    // Aggregate path: the row loads fine below usable(), so it
    // lists and status --all renders the load error, not a skip.
    // (localhost always lists — materialized or synthesized.)
    assert_eq!(list_clients().unwrap(), vec!["localhost", "noq"]);

    // localhost held to the same rule: a hand-emptied qroot on the
    // on-disk row rejects instead of loopbacking into cwd.
    let lo = load_client("localhost").unwrap();
    save_client(&ClientEntry { qroot: String::new(), ..lo }).unwrap();
    assert!(load_client("localhost").is_err());

    std::fs::remove_dir_all(&tmp).ok();
}
