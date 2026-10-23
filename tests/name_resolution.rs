//! Name-resolution middleware at the CLI surface (design.md §4.1):
//! typed `-c`/`--queue`/`--members` names resolve exact → unique
//! case-fold (substituted, announced on stderr) → prefix/fuzzy
//! did-you-mean suggestions — which are never auto-accepted.

mod common;

use common::bin;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("shunt-names-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn run(home: &Path, args: &[&str]) -> Output {
    Command::new(bin()).args(args).env("SHUNT_HOME", home).output().expect("shunt run failed")
}

fn ok(home: &Path, args: &[&str]) -> Output {
    let o = run(home, args);
    assert!(o.status.success(), "shunt {args:?} failed: {}", String::from_utf8_lossy(&o.stderr));
    o
}

fn err(home: &Path, args: &[&str]) -> String {
    let o = run(home, args);
    assert!(!o.status.success(), "shunt {args:?} unexpectedly ok: {}", String::from_utf8_lossy(&o.stdout));
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn write_client(home: &Path, name: &str, extra: &str) {
    std::fs::create_dir_all(home.join("clients")).unwrap();
    std::fs::write(home.join("clients").join(format!("{name}.toml")), format!("name = \"{name}\"\nqroot = \"/root/q\"\ndeploy_root = \"/root\"\n{extra}")).unwrap();
}

/// A canonical `[queues.<name>]` row — top-level qroot must stay
/// empty or the row trips the mixed-shape rejection.
fn write_queued_client(home: &Path, name: &str, queue: &str) {
    std::fs::create_dir_all(home.join("clients")).unwrap();
    std::fs::write(home.join("clients").join(format!("{name}.toml")), format!("name = \"{name}\"\ndeploy_root = \"/root\"\n[queues.{queue}]\nqroot = \"/root/{queue}\"\n")).unwrap();
}

/// `-c` case-folds to the registered client and says so on stderr;
/// `tunnel status` is the network-free -c op that shows it.
#[test]
fn c_flag_canonicalizes_and_announces() {
    let home = tmpdir("canon");
    write_client(&home, "Box-7", "");
    let o = ok(&home, &["tunnel", "status", "-c", "box-7"]);
    let e = String::from_utf8_lossy(&o.stderr);
    assert!(e.contains("resolved 'box-7' → client 'Box-7'"), "{e}");
    assert!(String::from_utf8_lossy(&o.stdout).contains("Box-7: down"));
    std::fs::remove_dir_all(&home).ok();
}

/// A miss is a loud error with did-you-mean suggestions — prefix
/// hits included but never accepted (unlike room names).
#[test]
fn unknown_client_carries_suggestions() {
    let home = tmpdir("suggest");
    write_client(&home, "gpu-a", "");
    write_client(&home, "gpu-b", "");
    let e = err(&home, &["tunnel", "status", "-c", "gpu"]);
    assert!(e.contains("unknown client \"gpu\"") && e.contains("did you mean: gpu-a, gpu-b"), "{e}");
    // Fuzzy hits suggest too — a 1-edit typo lands in the list.
    let e = err(&home, &["tunnel", "status", "-c", "gpu-c"]);
    assert!(e.contains("did you mean"), "{e}");
    // Case-variant duplicates are a loud ambiguity, not a coin flip.
    write_client(&home, "GPU-A", "");
    let e = err(&home, &["tunnel", "status", "-c", "Gpu-A"]);
    assert!(e.contains("ambiguous") && e.contains("GPU-A") && e.contains("gpu-a"), "{e}");
    std::fs::remove_dir_all(&home).ok();
}

/// A disabled row resolves as unknown to the middleware (listings
/// skip it) but still gets the truer "disabled" error.
#[test]
fn disabled_client_still_says_disabled() {
    let home = tmpdir("disabled");
    write_client(&home, "dead", "disabled = true\n");
    let e = err(&home, &["tunnel", "status", "-c", "dead"]);
    assert!(e.contains("disabled"), "{e}");
    std::fs::remove_dir_all(&home).ok();
}

/// `--queue` runs the same ladder: a case-folded queue name
/// substitutes with a note naming the client.
#[test]
fn queue_flag_canonicalizes() {
    let home = tmpdir("queue");
    write_queued_client(&home, "gpu-a", "train");
    let o = ok(&home, &["queue", "show", "Train", "-c", "gpu-a"]);
    let e = String::from_utf8_lossy(&o.stderr);
    assert!(e.contains("resolved 'Train' → queue 'train'"), "{e}");
    assert!(String::from_utf8_lossy(&o.stdout).contains("\"name\": \"train\""));
    // A miss names the registered queues plus suggestions.
    let e = err(&home, &["queue", "show", "trai", "-c", "gpu-a"]);
    assert!(e.contains("unknown queue \"trai\"") && e.contains("train"), "{e}");
    std::fs::remove_dir_all(&home).ok();
}

/// `project bind <client>` resolves the client reference too and
/// stores the canonical spelling in the binding.
#[test]
fn project_bind_stores_canonical_client() {
    let home = tmpdir("bind");
    write_client(&home, "Box-7", "");
    ok(&home, &["project", "add", "p"]);
    let o = ok(&home, &["project", "bind", "p", "box-7", "--workdir", "/w/p"]);
    let e = String::from_utf8_lossy(&o.stderr);
    assert!(e.contains("resolved 'box-7' → client 'Box-7'"), "{e}");
    let row = std::fs::read_to_string(home.join("projects/p.toml")).unwrap();
    assert!(row.contains("client = \"Box-7\""), "{row}");
    std::fs::remove_dir_all(&home).ok();
}

/// `room create --members` canonicalizes against registered
/// projects and writes the canonical spelling to the roster.
#[test]
fn room_members_canonicalize() {
    let home = tmpdir("members");
    std::fs::create_dir_all(home.join("projects")).unwrap();
    std::fs::write(home.join("projects/RiboWorld.toml"), "name = \"RiboWorld\"\n").unwrap();
    let o = ok(&home, &["room", "create", "r", "--members", "riboworld"]);
    let e = String::from_utf8_lossy(&o.stderr);
    assert!(e.contains("member 'riboworld' → project 'RiboWorld'"), "{e}");
    let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(home.join("rooms/r/meta.json")).unwrap()).unwrap();
    assert_eq!(meta["members"], serde_json::json!(["RiboWorld"]));
    // An unregistered member still lands, with did-you-mean notes.
    let o = ok(&home, &["room", "create", "r2", "--members", "riboworl"]);
    let e = String::from_utf8_lossy(&o.stderr);
    assert!(e.contains("member 'riboworl' is not a registered project") && e.contains("did you mean: RiboWorld"), "{e}");
    std::fs::remove_dir_all(&home).ok();
}
