use super::remote::remote_render;
use super::{required_client, resolve_client};
use crate::server::registry::tests::HOME_LOCK;

fn tmp_home(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("shunt-cli-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn required_client_falls_back_to_default() {
    let _g = HOME_LOCK.lock().unwrap();
    let home = tmp_home("reqcli");
    std::env::set_var("SHUNT_HOME", &home);
    std::fs::write(home.join("server.toml"), "default_client = \"gpu-1\"\n").unwrap();
    // The -c value resolves through the registry name middleware,
    // so the row must exist for the flag path to return it.
    std::fs::create_dir_all(home.join("clients")).unwrap();
    std::fs::write(home.join("clients/other.toml"), "name = \"other\"\n").unwrap();
    assert_eq!(required_client(&None).unwrap(), "gpu-1");
    assert_eq!(required_client(&Some("other".into())).unwrap(), "other");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn required_client_errors_without_flag_or_default() {
    let _g = HOME_LOCK.lock().unwrap();
    let home = tmp_home("nocli");
    std::env::set_var("SHUNT_HOME", &home);
    let err = required_client(&None).unwrap_err().to_string();
    assert!(err.contains("default_client"), "{err}");
    std::fs::remove_dir_all(&home).ok();
}

/// Wire stdout buried in login-shell noise renders as the
/// pretty object; non-wire output (logs, ad-hoc text) stays
/// verbatim; a wrong-protocol object surfaces §5's drift error.
#[test]
fn remote_render_extracts_wire_from_noise() {
    let noisy = "Welcome to AutoDL\n{\"protocol\":1,\"ok\":true,\"name\":\"0100_x\"}\n";
    let r = remote_render(noisy, true).unwrap();
    assert!(r.contains("\"protocol\": 1") && r.contains("\"name\": \"0100_x\""), "{r}");
    assert!(!r.contains("Welcome"), "{r}");

    assert_eq!(remote_render("task log line\nmore\n", true).unwrap(), "task log line\nmore\n");
    assert_eq!(remote_render("no trailing nl", true).unwrap(), "no trailing nl\n");

    let err = remote_render("banner\n{\"protocol\":2,\"ok\":true}\n", true).unwrap_err().to_string();
    assert!(err.contains("protocol mismatch"), "{err}");

    // A pre-v1 remote answering a bare JSON object (no `protocol`)
    // is drift too — the §5 steering error must surface, not a
    // verbatim print that exits 0.
    let err = remote_render("{\"ok\":true,\"name\":\"0100_x\"}\n", true).unwrap_err().to_string();
    assert!(err.contains("client bootstrap"), "{err}");

    // Garbage around a protocol-bearing stale answer still errors.
    let err = remote_render("\x1b[0mgarbage\n{\"protocol\":2}\ntrailing", true).unwrap_err().to_string();
    assert!(err.contains("protocol mismatch"), "{err}");
}

/// Text ops (`logs`) never touch extract: a task log containing a
/// protocol-shaped line prints verbatim — it is neither collapsed
/// to that object nor read as binary drift (design.md §5).
#[test]
fn remote_render_text_op_prints_verbatim() {
    let log = "epoch 1 loss 0.5\n{\"protocol\":1,\"ok\":true}\nepoch 2 loss 0.4\n";
    assert_eq!(remote_render(log, false).unwrap(), log);

    let drift_looking = "epoch 1\n{\"protocol\":2,\"ok\":true}\n";
    assert_eq!(remote_render(drift_looking, false).unwrap(), drift_looking);

    assert_eq!(remote_render("log without newline", false).unwrap(), "log without newline\n");
}

#[test]
fn resolve_client_respects_local_scope() {
    let _g = HOME_LOCK.lock().unwrap();
    let home = tmp_home("rescli");
    std::env::set_var("SHUNT_HOME", &home);
    std::env::remove_var("SHUNT_ROOT");
    std::fs::write(home.join("server.toml"), "default_client = \"gpu-1\"\n").unwrap();
    // No flag + no local queue -> default_client.
    assert_eq!(resolve_client(&None, &None).unwrap().as_deref(), Some("gpu-1"));
    // A local root in scope keeps the call local.
    assert_eq!(resolve_client(&None, &Some("/tmp/q".into())).unwrap(), None);
    // Explicit -c always wins — resolved through the name
    // middleware, so the row must be registered.
    std::fs::create_dir_all(home.join("clients")).unwrap();
    std::fs::write(home.join("clients/x.toml"), "name = \"x\"\n").unwrap();
    assert_eq!(resolve_client(&Some("x".into()), &Some("/tmp/q".into())).unwrap().as_deref(), Some("x"));
    std::fs::remove_dir_all(&home).ok();
}
