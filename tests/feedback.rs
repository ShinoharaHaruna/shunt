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
fn scrubs_registered_names_from_title_body_and_context() {
    let home = tmpdir("names");
    std::fs::create_dir_all(home.join("clients")).unwrap();
    std::fs::create_dir_all(home.join("projects")).unwrap();
    // A hyphenated client carrying a named queue, the exempt
    // loopback peer on disk, an exempt generic name, and a
    // mixed-case project.
    std::fs::write(home.join("clients/pytorch-7032.toml"), "name = \"pytorch-7032\"\ndeploy_root = \"/root\"\n[queues.gpu-queue]\nqroot = \"/root/q/gpu\"\n").unwrap();
    std::fs::write(home.join("clients/localhost.toml"), "name = \"localhost\"\ndeploy_root = \"/home/u\"\n").unwrap();
    std::fs::write(home.join("clients/admin.toml"), "name = \"admin\"\ndeploy_root = \"/root\"\n").unwrap();
    std::fs::write(home.join("projects/RiboWorld.toml"), "name = \"RiboWorld\"\n").unwrap();
    let stub = stub_gh(&home);
    let path = format!("{}:{}", stub.display(), std::env::var("PATH").unwrap());
    let env = [("PATH", path.as_str()), ("FBSTUB", home.to_str().unwrap())];

    let o = run(&home, &["feedback", "RiboWorld run on pytorch-7032 died", "--body", "PYTORCH-7032 lost gpu-queue for riboworld-waves; localhost and admin stay readable", "-c", "pytorch-7032"], &env);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(String::from_utf8_lossy(&o.stderr).contains("redacted"));
    // Title ships in gh argv — kind-tagged placeholders there too.
    let args = std::fs::read_to_string(home.join("args.log")).unwrap();
    assert!(args.contains("[feedback/bug] <PROJECT_NAME> run on <BOX_NAME> died"), "{args}");
    let body = std::fs::read_to_string(home.join("last_body.txt")).unwrap();
    let low = body.to_lowercase();
    for leak in ["pytorch-7032", "riboworld", "gpu-queue"] {
        assert!(!low.contains(leak), "delivered body leaks {leak}: {body}");
    }
    assert!(body.contains("<BOX_NAME> lost <QUEUE_NAME> for <PROJECT_NAME>-waves"), "{body}");
    // Reserved/generic names pass through untouched.
    assert!(body.contains("localhost"), "{body}");
    assert!(body.contains("admin"), "{body}");
    // The --client context block ships the name redacted as well.
    assert!(body.contains("\"name\": \"<BOX_NAME>\""), "{body}");
    // The persisted artifact is the same scrubbed text.
    let stored = std::fs::read_to_string(&artifacts(&home)[0]).unwrap();
    assert!(!stored.to_lowercase().contains("pytorch"), "{stored}");
    assert!(stored.contains("<PROJECT_NAME>"), "{stored}");
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

fn register_synthetic_secret(home: &Path) {
    std::fs::create_dir_all(home.join("clients")).unwrap();
    std::fs::write(home.join("clients/synthetic.toml"), "[ssh]\npassword = \"synthetic-secret-9876\"\n").unwrap();
}

#[test]
fn sanitized_title_stays_private_when_delivery_disabled() {
    for json in [false, true] {
        let home = tmpdir(if json { "title-private-json" } else { "title-private-human" });
        register_synthetic_secret(&home);
        let mut args = vec!["feedback", "connection with synthetic-secret-9876", "--body", "boom synthetic-secret-9876"];
        if json {
            args.push("--json");
        }
        let o = run(&home, &args, &[("SHUNT_FEEDBACK_REPO", "")]);
        assert!(!o.status.success());
        let stdout = String::from_utf8_lossy(&o.stdout);
        let stderr = String::from_utf8_lossy(&o.stderr);
        assert!(!stdout.contains("synthetic-secret-9876"), "stdout leaks: {stdout}");
        assert!(!stderr.contains("synthetic-secret-9876"), "stderr leaks: {stderr}");
        assert!(stderr.contains("redacted 2 sensitive value(s)"), "{stderr}");
        let files = artifacts(&home);
        assert_eq!(files.len(), 1);
        let path = files[0].to_str().unwrap();
        assert!(!path.contains("synthetic-secret-9876"), "path leaks: {path}");
        let stored = std::fs::read_to_string(&files[0]).unwrap();
        assert!(!stored.contains("synthetic-secret-9876"), "artifact leaks: {stored}");
        let art: serde_json::Value = serde_json::from_str(&stored).unwrap();
        assert_eq!(art["title"], "connection with <redacted>");
        assert!(art["body"].as_str().unwrap().starts_with("boom <redacted>\n\n---\n"));
        if json {
            let result: serde_json::Value = serde_json::from_str(&stdout).unwrap();
            assert_eq!(result["artifact"], path);
        }
        for args in [vec!["feedback", "ls"], vec!["feedback", "ls", "--json"]] {
            let o = run(&home, &args, &[("SHUNT_FEEDBACK_REPO", "")]);
            assert!(o.status.success());
            let stdout = String::from_utf8_lossy(&o.stdout);
            let stderr = String::from_utf8_lossy(&o.stderr);
            assert!(!stdout.contains("synthetic-secret-9876"), "ls leaks: {stdout}");
            assert!(!stderr.contains("synthetic-secret-9876"), "ls stderr leaks: {stderr}");
            assert!(stdout.contains("connection with <redacted>"), "{stdout}");
        }
    }
}

#[test]
fn sanitized_title_matches_initial_delivery_retry_and_dedup() {
    let home = tmpdir("title-dedup");
    register_synthetic_secret(&home);
    let args = ["feedback", "connection with synthetic-secret-9876", "--body", "boom", "--json"];
    let o = run(&home, &args, &[("SHUNT_FEEDBACK_REPO", "")]);
    assert!(!o.status.success());
    let pending = artifacts(&home).pop().unwrap();

    let stub = stub_gh(&home);
    let path = format!("{}:{}", stub.display(), std::env::var("PATH").unwrap());
    let env = [("PATH", path.as_str()), ("FBSTUB", home.to_str().unwrap()), ("SHUNT_FEEDBACK_REPO", "stub/repo")];
    let o = run(&home, &args, &env);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let result: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(result["deduped"], false);
    let delivered_args = std::fs::read_to_string(home.join("args.log")).unwrap();
    assert!(delivered_args.contains("--title [feedback/bug] connection with <redacted> --body-file -"), "{delivered_args}");
    assert!(!delivered_args.contains("synthetic-secret-9876"));

    // Retry must compare the same scrubbed title as the initial send.
    let o = run(&home, &["feedback", "retry", pending.file_stem().unwrap().to_str().unwrap(), "--json"], &env);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let result: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(result["deduped"], true);
    let o = run(&home, &["feedback", "connection with <redacted>", "--body", "boom", "--json"], &env);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let result: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(result["deduped"], true);
    assert_eq!(std::fs::read_to_string(home.join("args.log")).unwrap().lines().count(), 1);
    for file in artifacts(&home) {
        let art: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
        assert_eq!(art["title"], "connection with <redacted>");
        assert_eq!(art["issue_number"], 7);
    }
}

#[test]
fn concurrent_reports_preserve_every_distinct_body() {
    use std::collections::HashSet;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Stdio;

    let home = tmpdir("concurrent");
    let mut children = Vec::new();
    // Each process blocks on its own pipe until all 64 have been spawned.
    for i in 0..64 {
        let body = format!("unique concurrent report {i}");
        let child = Command::new("/bin/sh").args(["-c", "read gate; exec \"$@\"", "feedback-gate"]).arg(bin()).args(["feedback", "same concurrent title", "--body", &body, "--json"]).env("SHUNT_HOME", &home).env("SHUNT_FEEDBACK_REPO", "").env("SHUNT_FEEDBACK_MAX_PER_HOUR", "100").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        children.push((body, child));
    }
    for (_, child) in &mut children {
        child.stdin.take().unwrap().write_all(b"go\n").unwrap();
    }
    let mut paths = HashSet::new();
    for (body, child) in children {
        let o = child.wait_with_output().unwrap();
        assert!(!o.status.success());
        let err = String::from_utf8_lossy(&o.stderr);
        assert!(err.contains("delivery disabled"), "{err}");
        assert!(!err.contains("warning:"), "{err}");
        let result: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
        let path = PathBuf::from(result["artifact"].as_str().expect("report must persist"));
        assert!(paths.insert(path.clone()), "artifact name reused: {}", path.display());
        let art: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(art["title"], "same concurrent title");
        assert!(art["body"].as_str().unwrap().starts_with(&format!("{body}\n\n---\n")));
        assert!(art["delivered_at"].is_null());
        assert_eq!(art["attempts"], 0);
        assert_eq!(std::fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o600);
    }
    assert_eq!(artifacts(&home).len(), 64, "no leftover staging files");
    let o = run(&home, &["feedback", "ls", "--json"], &[("SHUNT_FEEDBACK_REPO", "")]);
    assert!(o.status.success());
    assert!(o.stderr.is_empty(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(String::from_utf8_lossy(&o.stdout).lines().count(), 64);
    std::fs::remove_dir_all(home).unwrap();
}

fn stage_retry_artifact(home: &Path, id: &str, title: &str) {
    let o = run(home, &["feedback", title, "--body", title, "--json"], &[("SHUNT_FEEDBACK_REPO", "")]);
    assert!(!o.status.success());
    let result: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    std::fs::rename(result["artifact"].as_str().unwrap(), home.join("feedback").join(format!("{id}.json"))).unwrap();
}

fn retry_snapshot(home: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    artifacts(home)
        .into_iter()
        .map(|p| {
            let bytes = std::fs::read(&p).unwrap();
            (p, bytes)
        })
        .collect()
}

fn gh_calls(home: &Path) -> usize {
    std::fs::read_to_string(home.join("args.log")).unwrap_or_default().lines().count()
}

#[test]
fn retry_ambiguous_prefix_has_no_side_effects() {
    let home = tmpdir("retry-ambiguous");
    stage_retry_artifact(&home, "1700000000_alpha", "alpha report");
    stage_retry_artifact(&home, "1700000000_beta", "beta report");
    let before = retry_snapshot(&home);
    let stub = stub_gh(&home);
    let path = format!("{}:{}", stub.display(), std::env::var("PATH").unwrap());
    let env = [("PATH", path.as_str()), ("FBSTUB", home.to_str().unwrap()), ("SHUNT_FEEDBACK_REPO", "stub/repo")];
    for json in [false, true] {
        let mut args = vec!["feedback", "retry", "1700000000"];
        if json {
            args.push("--json");
        }
        let o = run(&home, &args, &env);
        assert_eq!(gh_calls(&home), 0, "ambiguous retry must not call gh; status={}, stderr={}", o.status, String::from_utf8_lossy(&o.stderr));
        assert!(!o.status.success());
        let err = String::from_utf8_lossy(&o.stderr);
        for hint in ["ambiguous", "1700000000_alpha", "1700000000_beta", "shunt feedback retry"] {
            assert!(err.contains(hint), "missing {hint:?}: {err}");
        }
        assert_eq!(retry_snapshot(&home), before, "selection errors must not mutate attempts or state");
    }
    std::fs::remove_dir_all(home).unwrap();
}

#[test]
fn retry_resolves_exact_unique_and_all_with_delivered_candidates() {
    let home = tmpdir("retry-selection");
    for (id, title) in [("1700000000_alpha", "alpha report"), ("1700000000_alpha_extra", "extra report"), ("1700000000_beta", "beta report"), ("1800000000_other", "other report")] {
        stage_retry_artifact(&home, id, title);
    }
    let stub = stub_gh(&home);
    let path = format!("{}:{}", stub.display(), std::env::var("PATH").unwrap());
    let env = [("PATH", path.as_str()), ("FBSTUB", home.to_str().unwrap()), ("SHUNT_FEEDBACK_REPO", "stub/repo"), ("SHUNT_FEEDBACK_MAX_PER_HOUR", "20")];
    // An exact stem wins even when it also prefixes another pending artifact.
    let o = run(&home, &["feedback", "retry", "1700000000_alpha", "--json"], &env);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(gh_calls(&home), 1);
    let result: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(result["id"], "1700000000_alpha");

    for (id, hints) in [("1700000000_al", vec!["ambiguous", "1700000000_alpha", "1700000000_alpha_extra", "shunt feedback retry"]), ("1700000000_alpha", vec!["already filed", "https://github.com/stub/repo/issues/7", "shunt feedback ls"]), ("missing", vec!["no feedback artifact", "shunt feedback ls"])] {
        let before = retry_snapshot(&home);
        let o = run(&home, &["feedback", "retry", id], &env);
        assert!(!o.status.success());
        let err = String::from_utf8_lossy(&o.stderr);
        for hint in hints {
            assert!(err.contains(hint), "missing {hint:?}: {err}");
        }
        assert_eq!(gh_calls(&home), 1);
        assert_eq!(retry_snapshot(&home), before);
    }
    // A unique delivered prefix must not be mistaken for a nonexistent id.
    let o = run(&home, &["feedback", "retry", "1700000000_b"], &env);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(gh_calls(&home), 2);
    let before = retry_snapshot(&home);
    let o = run(&home, &["feedback", "retry", "1700000000_b"], &env);
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("already filed"));
    assert_eq!(retry_snapshot(&home), before);
    assert_eq!(gh_calls(&home), 2);

    let o = run(&home, &["feedback", "retry", "--json"], &env);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(String::from_utf8_lossy(&o.stdout).lines().count(), 2);
    assert_eq!(gh_calls(&home), 4);
    for (_, bytes) in retry_snapshot(&home) {
        let art: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(!art["delivered_at"].is_null());
        assert_eq!(art["attempts"], 1);
    }
    std::fs::remove_dir_all(home).unwrap();
}
