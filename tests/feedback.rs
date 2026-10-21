//! Feedback channel (design.md §14): persist-first ledger, sanitize,
//! dedup, rate cap, retry — against a stub `gh` on PATH, never the
//! real GitHub API. Each case gets a tmp $SHUNT_HOME.

mod common;

use common::bin;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("shunt-fb-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A `gh` stub: records argv per call, keeps the last stdin body,
/// answers with a fake issue URL.
fn stub_gh(dir: &Path) -> PathBuf {
    let bindir = dir.join("bin");
    std::fs::create_dir_all(&bindir).unwrap();
    let gh = bindir.join("gh");
    std::fs::write(&gh, "#!/usr/bin/env bash\necho \"$@\" >> \"$FBSTUB/args.log\"\ncat > \"$FBSTUB/last_body.txt\"\necho \"https://github.com/stub/repo/issues/7\"\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bindir
}

fn run(home: &Path, args: &[&str], extra: &[(&str, &str)]) -> Output {
    let mut c = Command::new(bin());
    c.args(args).env("SHUNT_HOME", home);
    for (k, v) in extra {
        c.env(k, v);
    }
    c.output().expect("shunt feedback run failed")
}

fn artifacts(home: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(home.join("feedback")).map(|rd| rd.flatten().map(|e| e.path()).collect()).unwrap_or_default();
    v.sort();
    v
}

#[test]
fn persists_pending_when_delivery_disabled() {
    let home = tmpdir("disabled");
    let o = run(&home, &["feedback", "daemon ate my task", "--body", "boom"], &[("SHUNT_FEEDBACK_REPO", "")]);
    assert!(!o.status.success());
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(err.contains("feedback not delivered"), "{err}");
    assert!(err.contains("shunt feedback retry"), "{err}");
    let files = artifacts(&home);
    assert_eq!(files.len(), 1);
    let art: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&files[0]).unwrap()).unwrap();
    assert_eq!(art["delivered_at"], serde_json::Value::Null);
    assert_eq!(art["title"], "daemon ate my task");
    // 0600 like every registry file.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&files[0]).unwrap().permissions().mode() & 0o777, 0o600);
    }
}

#[test]
fn delivers_via_gh_stub_and_dedups() {
    let home = tmpdir("deliver");
    let stub = stub_gh(&home);
    let path = format!("{}:{}", stub.display(), std::env::var("PATH").unwrap());
    let env = [("PATH", path.as_str()), ("FBSTUB", home.to_str().unwrap())];

    let o = run(&home, &["feedback", "queue ate task", "--body", "repro here"], &env);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(String::from_utf8_lossy(&o.stdout).contains("https://github.com/stub/repo/issues/7"));
    let args = std::fs::read_to_string(home.join("args.log")).unwrap();
    assert!(args.contains("--repo ShinoharaHaruna/shunt"), "{args}");
    assert!(args.contains("[feedback/bug] queue ate task"), "{args}");

    // Same report again: dedup short-circuits to the existing URL —
    // the stub must not see a second call.
    let o = run(&home, &["feedback", "queue ate task", "--body", "repro here"], &env);
    assert!(o.status.success());
    assert!(String::from_utf8_lossy(&o.stdout).contains("already filed"));
    assert_eq!(std::fs::read_to_string(home.join("args.log")).unwrap().lines().count(), 1);
}

#[test]
fn scrubs_registry_secrets_from_body() {
    let home = tmpdir("scrub");
    std::fs::create_dir_all(home.join("clients")).unwrap();
    std::fs::write(home.join("clients/rw-a.toml"), "name = \"rw-a\"\nqroot = \"/root/q\"\ndeploy_root = \"/root\"\n[ssh]\nhost = \"203.0.113.9\"\nport = 12345\nuser = \"root\"\npassword = \"s3cretpw\"\n[facts]\ngpu = \"RTX 4090\"\n").unwrap();
    let stub = stub_gh(&home);
    let path = format!("{}:{}", stub.display(), std::env::var("PATH").unwrap());
    let env = [("PATH", path.as_str()), ("FBSTUB", home.to_str().unwrap())];

    let o = run(&home, &["feedback", "conn died", "--body", "ssh root@203.0.113.9 -p 12345 refused, pw s3cretpw", "-c", "rw-a"], &env);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(String::from_utf8_lossy(&o.stderr).contains("redacted"));
    let body = std::fs::read_to_string(home.join("last_body.txt")).unwrap();
    for leak in ["203.0.113.9", "12345", "s3cretpw"] {
        assert!(!body.contains(leak), "delivered body leaks {leak}");
    }
    assert!(body.contains("RTX 4090"), "client facts missing: {body}");
    // The stored artifact is the scrubbed text too.
    let stored = std::fs::read_to_string(&artifacts(&home)[0]).unwrap();
    assert!(!stored.contains("s3cretpw"));
}

#[test]
fn rate_cap_persists_and_retry_delivers() {
    let home = tmpdir("cap");
    let stub = stub_gh(&home);
    let path = format!("{}:{}", stub.display(), std::env::var("PATH").unwrap());
    let env = [("PATH", path.as_str()), ("FBSTUB", home.to_str().unwrap()), ("SHUNT_FEEDBACK_MAX_PER_HOUR", "1")];

    let o = run(&home, &["feedback", "first", "--body", "b1"], &env);
    assert!(o.status.success());
    // Second real delivery in the same hour is refused but kept.
    let o = run(&home, &["feedback", "second", "--body", "b2"], &env);
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("rate cap"));
    assert_eq!(artifacts(&home).len(), 2);

    // `ls` shows one filed, one pending.
    let o = run(&home, &["feedback", "ls", "--json"], &env);
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(out.contains("\"delivered\":true") && out.contains("\"delivered\":false"), "{out}");

    // `retry` honors the cap too — it is the cap's own remediation,
    // not a bypass — so while the cap window is hot it stays pending.
    let o = run(&home, &["feedback", "retry"], &env);
    assert!(!o.status.success());

    // With headroom, retry delivers it.
    let env2 = [("PATH", path.as_str()), ("FBSTUB", home.to_str().unwrap()), ("SHUNT_FEEDBACK_MAX_PER_HOUR", "9")];
    let o = run(&home, &["feedback", "retry"], &env2);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(std::fs::read_to_string(home.join("args.log")).unwrap().lines().count(), 2);
    let o = run(&home, &["feedback", "ls", "--json"], &env);
    assert!(!String::from_utf8_lossy(&o.stdout).contains("\"delivered\":false"));
}
