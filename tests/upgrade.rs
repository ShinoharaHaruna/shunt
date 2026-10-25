//! `shunt upgrade --check` against a stub `gh` on PATH — the read-only
//! path reports latest-vs-current honestly; nothing downloads.

mod common;

use std::process::Command;

/// A `gh` stub answering `release view` with the tag in $GH_TAG;
/// any other gh call fails so accidental fetches stay loud.
fn stub_gh(dir: &std::path::Path) -> std::path::PathBuf {
    let gh = dir.join("gh");
    std::fs::write(&gh, "#!/usr/bin/env bash\nif [ \"$1\" = \"release\" ] && [ \"$2\" = \"view\" ]; then echo \"$GH_TAG\"; else exit 1; fi\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    dir.to_path_buf()
}

fn check(home: &std::path::Path, tag: &str) -> serde_json::Value {
    let stub = stub_gh(home);
    let out = Command::new(common::bin()).args(["upgrade", "--check", "--json"]).env("SHUNT_HOME", home).env("GH_TAG", tag).env("PATH", format!("{}:{}", stub.display(), std::env::var("PATH").unwrap())).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

/// A local row has no box binary (design.md §16): `upgrade -c`
/// reports the shared dev-machine image as an explicit no-op —
/// never a compat error from smoking a <qroot>/bin/shunt that was
/// never deployed.
#[test]
fn upgrade_client_on_local_row_is_explicit_noop() {
    let home = std::env::temp_dir().join(format!("shunt-upgradelocal-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let stub = stub_gh(&home);
    let cmd = || {
        let mut c = Command::new(common::bin());
        c.env("SHUNT_HOME", &home).env("GH_TAG", "v99.0.0").env("PATH", format!("{}:{}", stub.display(), std::env::var("PATH").unwrap()));
        c
    };

    let out = cmd().args(["upgrade", "-c", "localhost", "--check", "--json"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let j: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(j["local"], true);
    assert_eq!(j["newer"], true);
    assert!(j.get("compat").is_none(), "local check must not report compat: {j}");

    let out = cmd().args(["upgrade", "-c", "localhost"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("nothing to upgrade"), "{text}");

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn upgrade_check_reports_newer_and_current() {
    let home = std::env::temp_dir().join(format!("shunt-upgradecheck-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();

    let j = check(&home, "v99.0.0");
    assert_eq!(j["latest"], "99.0.0");
    assert_eq!(j["newer"], true);

    let j = check(&home, "v0.0.1");
    assert_eq!(j["newer"], false);

    std::fs::remove_dir_all(&home).ok();
}
