//! Wire error contract (design.md §5): every `--json` op that fails
//! must still print `{"protocol":1,"ok":false,"error":...}` on
//! stdout and exit nonzero. The server extracts that object from
//! remote stdout; a bare `?` propagation prints `shunt: <err>` to
//! stderr only, which the server reports as "no wire JSON object"
//! instead of the real error.

mod common;

use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    let mut c = Command::new(common::bin());
    c.args(args).env_remove("SHUNT_ROOT");
    c.output().expect("shunt run failed")
}

fn assert_wire_error(o: &Output, args: &[&str]) {
    assert!(!o.status.success(), "shunt {args:?} unexpectedly ok");
    let stdout = String::from_utf8_lossy(&o.stdout);
    let v = shunt::proto::wire::extract(&stdout).unwrap_or_else(|e| panic!("shunt {args:?}: no wire object on stdout ({e}); stdout={stdout:?}"));
    assert_eq!(v["ok"], false, "shunt {args:?}: wire object not an error: {v}");
    assert!(v["error"].as_str().is_some_and(|s| !s.is_empty()), "shunt {args:?}: empty error field: {v}");
}

/// A `--root` naming a regular file fails deep inside the handler
/// (read_dir ENOTDIR, write ENOTDIR) — the paths that used to
/// propagate `?` bare past `jfail`.
#[test]
fn json_ops_emit_wire_error_object() {
    let file = std::env::temp_dir().join(format!("shunt-wire-{}", std::process::id()));
    std::fs::write(&file, "not a queue root\n").unwrap();
    let root = file.display().to_string();
    let cases: Vec<Vec<&str>> = vec![vec!["status", "--json", "--root", &root], vec!["workers", "--json", "--root", &root, "set", "5"], vec!["shutdown", "--json", "--root", &root, "on"], vec!["stop", "--json", "--root", &root], vec!["init", "--json", "--root", &root], vec!["enqueue", "--json", "--root", &root, "-", "x"], vec!["new", "--json", "--root", &root, "--slug", "x", "--", "true"], vec!["cancel", "--json", "--root", &root, "pat"], vec!["prio", "--json", "--root", &root, "pat", "5"], vec!["kill", "--json", "--root", &root, "pat"]];
    for args in &cases {
        assert_wire_error(&run(args), args);
    }
    std::fs::remove_file(&file).ok();
}

/// Without `--json` the same failure stays human-facing: nonzero
/// exit, `shunt: <err>` on stderr, no wire object on stdout.
#[test]
fn non_json_failure_stays_human_facing() {
    let file = std::env::temp_dir().join(format!("shunt-wire-h-{}", std::process::id()));
    std::fs::write(&file, "not a queue root\n").unwrap();
    let root = file.display().to_string();
    let args = ["status", "--root", &root];
    let o = run(&args);
    assert!(!o.status.success());
    assert!(shunt::proto::wire::extract(&String::from_utf8_lossy(&o.stdout)).is_err());
    assert!(String::from_utf8_lossy(&o.stderr).contains("shunt:"));
    std::fs::remove_file(&file).ok();
}

/// `workers 3 set 5` / `workers 3 get` are ambiguous — the dispatch
/// rejects positional N + subcommand together, on both the local
/// path and the `-c box` wire-forward path (before any ssh or
/// registry lookup, so no fixture is needed here).
#[test]
fn workers_rejects_positional_plus_subcommand() {
    let cases: [&[&str]; 3] = [&["workers", "3", "set", "5"], &["workers", "3", "get"], &["workers", "3", "set", "5", "-c", "box"]];
    for args in cases {
        let o = run(args);
        assert!(!o.status.success(), "shunt {args:?} unexpectedly ok");
        let stderr = String::from_utf8_lossy(&o.stderr);
        assert!(stderr.contains("not both"), "shunt {args:?}: {stderr}");
    }
}
