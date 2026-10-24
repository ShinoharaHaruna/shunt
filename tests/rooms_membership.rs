//! Coordination rooms (design.md §17.3): the `--as` label contract —
//! open trust for off-roster labels, case-insensitive
//! canonicalization against registered projects, and the reserved
//! `admin` operator identity.

mod common;

use common::room::*;

/// A say whose --as is off the roster still posts (open trust) but
/// is recorded member:false and names the roster on stderr (§17.3).
#[test]
fn non_member_say_is_flagged() {
    let home = tmpdir("nonmember");
    ok(&home, &["room", "create", "r", "--members", "p1,p2"]);
    let o = run(&home, &["room", "say", "r", "--body", "hi", "--as", "intruder"], &[]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let e = String::from_utf8_lossy(&o.stderr);
    assert!(e.contains("roster: p1, p2"), "{e}");
    let msg: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(home.join("rooms/r/msgs/0001.json")).unwrap()).unwrap();
    assert_eq!(msg["member"], false);
    assert_eq!(msg["author"], "intruder");
    std::fs::remove_dir_all(&home).ok();
}

/// A `read` by a label off the roster still returns the stream and
/// advances the cursor — open trust — but names the roster on
/// stderr, the same warning style as `say`/`decide` (§17.2/§17.3).
#[test]
fn non_member_read_warns_but_returns() {
    let home = tmpdir("read-nonmember");
    ok(&home, &["room", "create", "r", "--members", "p1"]);
    ok(&home, &["room", "say", "r", "--body", "secret-results", "--as", "p1"]);
    let o = run(&home, &["room", "read", "r", "--as", "intruder"], &[]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let e = String::from_utf8_lossy(&o.stderr);
    assert!(e.contains("not a member") && e.contains("roster: p1"), "{e}");
    assert!(String::from_utf8_lossy(&o.stdout).contains("secret-results"));
    assert_eq!(last_read(&home, "r", "intruder"), 1, "a warned read still marks");
    std::fs::remove_dir_all(&home).ok();
}

/// `--as` canonicalizes case-insensitively against registered
/// project names (§17.3): `riboworld` becomes `RiboWorld` — a
/// member:true post, one author spelling, one cursor file. A
/// non-registered roster name still matches case-insensitively,
/// and two projects differing only by case are a loud ambiguity.
#[test]
fn as_label_canonicalizes_to_registered_project() {
    let home = tmpdir("canon");
    write_project(&home, "RiboWorld", &[]);
    ok(&home, &["room", "create", "r", "--members", "RiboWorld"]);
    let o = run(&home, &["room", "say", "r", "--body", "ack", "--as", "riboworld", "--json"], &[]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["member"], true, "mis-cased --as must land as the member");
    let msg: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(home.join("rooms/r/msgs/0001.json")).unwrap()).unwrap();
    assert_eq!(msg["author"], "RiboWorld");
    assert_eq!(msg["member"], true);
    // One canonical cursor — no readers/riboworld.json fragment.
    assert_eq!(last_read(&home, "r", "RiboWorld"), 1);
    assert!(!home.join("rooms/r/readers/riboworld.json").exists());

    // A roster holding a NON-registered name still matches --as
    // case-insensitively, and `ls` scopes to it.
    ok(&home, &["room", "create", "r2", "--members", "GERNA"]);
    let o = run(&home, &["room", "say", "r2", "--body", "hi", "--as", "gerna", "--json"], &[]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(!String::from_utf8_lossy(&o.stderr).contains("not a member"));
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&o.stdout).unwrap()["member"], true);
    let out = ok(&home, &["room", "ls", "--as", "gerna", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(v["rooms"].as_array().unwrap().iter().any(|r| r["name"] == "r2"), "{out}");

    // Two projects differing only by case: loud ambiguity, not a
    // coin flip.
    write_project(&home, "riboworld", &[]);
    let e = err(&home, &["room", "say", "r", "--body", "x", "--as", "RIBOWORLD"]);
    assert!(e.contains("ambiguous") && e.contains("RiboWorld"), "{e}");
    std::fs::remove_dir_all(&home).ok();
}

/// `admin` is the reserved operator identity (§17.3): no --as means
/// the operator — an implicit member of every room (member:true
/// posts, `ls` lists all rooms, no warnings) who can never land on
/// a roster or become a registered project.
#[test]
fn admin_is_the_reserved_operator_identity() {
    let home = tmpdir("admin");
    ok(&home, &["room", "create", "r", "--members", "p1"]);
    ok(&home, &["room", "create", "r3", "--members", "p2"]);
    ok(&home, &["room", "say", "r", "--body", "m1", "--as", "p1"]);

    // No --as: read and decide produce no non-member warning, and
    // the cursor lands on readers/admin.json.
    let o = run(&home, &["room", "read", "r"], &[]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(!String::from_utf8_lossy(&o.stderr).contains("not a member"));
    assert_eq!(last_read(&home, "r", "admin"), 1);
    let o = run(&home, &["room", "decide", "r", "--note", "ops call"], &[]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(!String::from_utf8_lossy(&o.stderr).contains("not a member"));
    let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(home.join("rooms/r/meta.json")).unwrap()).unwrap();
    assert_eq!(meta["decision"]["author"], "admin");

    // Any case variant folds to `admin`: one author, one cursor.
    // An implicit member posts member:true — the operator's seat is
    // real, it just never lands on a roster.
    let o = run(&home, &["room", "say", "r", "--body", "hi", "--as", "Admin", "--json"], &[]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(!String::from_utf8_lossy(&o.stderr).contains("not a member"));
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&o.stdout).unwrap()["member"], true);
    let msg: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(home.join("rooms/r/msgs/0002.json")).unwrap()).unwrap();
    assert_eq!(msg["author"], "admin");
    assert!(!home.join("rooms/r/readers/Admin.json").exists());
    let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(home.join("rooms/r/meta.json")).unwrap()).unwrap();
    assert_eq!(meta["members"], serde_json::json!(["p1"]), "admin never lands on a roster");

    // The implicit seat makes admin's `ls` inbox the whole
    // directory — no --all needed for the overseer.
    let out = ok(&home, &["room", "ls"]);
    assert!(out.lines().any(|l| l.split_whitespace().next() == Some("r")), "admin ls sees every room: {out}");
    assert!(out.lines().any(|l| l.split_whitespace().next() == Some("r3")), "{out}");

    // The reserved name is rejected at every intake that could
    // mint it as a project identity.
    for bad in ["admin", "Admin"] {
        let e = err(&home, &["project", "add", bad]);
        assert!(e.contains("reserved"), "{e}");
    }
    let e = err(&home, &["room", "create", "r2", "--members", "p1,admin"]);
    assert!(e.contains("reserved"), "{e}");
    assert!(!home.join("rooms/r2").exists());
    std::fs::remove_dir_all(&home).ok();
}
