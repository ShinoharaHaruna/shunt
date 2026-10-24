//! Coordination rooms (design.md §17): room-level archive — one
//! rename moves the whole record to `rooms/.archive/`, out of `ls`
//! and every op that resolves a room, with `unarchive`/`rm` as the
//! way back or out.

mod common;

use common::room::*;

/// Room-level archive (§17): one rename moves the whole record to
/// rooms/.archive/ — the room drops out of `ls` (into the
/// `archived` line/field), and every op that resolves a room fails
/// naming `unarchive`. Prefix matching stays live-only.
#[test]
fn archive_hides_from_live_and_ops_name_unarchive() {
    let home = tmpdir("roomarchive");
    ok(&home, &["room", "create", "vram", "--members", "p1,p2"]);
    ok(&home, &["room", "say", "vram", "--body", "cap 12G?", "--as", "p1"]);
    ok(&home, &["room", "archive", "vram"]);
    assert!(!home.join("rooms/vram").exists(), "the live dir moved wholesale");
    assert!(home.join("rooms/.archive/vram/meta.json").exists());
    assert!(home.join("rooms/.archive/vram/msgs/0001.json").exists());
    assert!(home.join("rooms/.archive/vram/readers/p1.json").exists(), "cursors ride along");

    // ls: gone from the rooms table, present in the archived line.
    let out = ok(&home, &["room", "ls", "--all"]);
    assert!(!out.lines().any(|l| l.starts_with("vram")), "{out}");
    assert!(out.contains("archived: vram"), "{out}");
    let out = ok(&home, &["room", "ls", "--all", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["archived"], serde_json::json!(["vram"]), "{out}");
    assert!(v["rooms"].as_array().unwrap().is_empty(), "{out}");

    // Second room: the archived line joins names.
    ok(&home, &["room", "create", "vram2", "--members", "p1"]);
    ok(&home, &["room", "archive", "vram2"]);
    let out = ok(&home, &["room", "ls", "--all"]);
    assert!(out.contains("archived: vram, vram2"), "{out}");

    // Every op that resolves a room fails with the restore path.
    for args in [vec!["room", "say", "vram", "--body", "x", "--as", "p1"], vec!["room", "read", "vram", "--as", "p1"], vec!["room", "decide", "vram", "--note", "x", "--as", "p1"], vec!["room", "members", "vram"], vec!["room", "compact", "vram", "--through", "1", "--body", "d", "--as", "p1"], vec!["room", "archive", "vram"]] {
        let e = err(&home, &args);
        assert!(e.contains("archived") && e.contains("unarchive"), "{args:?}: {e}");
    }
    // Prefix matching never reaches cold storage: 'vr' is a miss,
    // not an archived-room hit.
    let e = err(&home, &["room", "read", "vr", "--as", "p1"]);
    assert!(e.contains("no room"), "{e}");
    std::fs::remove_dir_all(&home).ok();
}

/// `unarchive` restores the whole record — meta, msgs, readers —
/// and the round trip preserves unread cursors: a say posted
/// before archiving is still unread/readable after it.
#[test]
fn unarchive_restores_record_and_cursors() {
    let home = tmpdir("unarchive");
    ok(&home, &["room", "create", "vram", "--members", "p1,p2"]);
    ok(&home, &["room", "say", "vram", "--body", "cap 12G?", "--as", "p1"]);
    ok(&home, &["room", "decide", "vram", "--note", "12G each", "--as", "p1"]);
    ok(&home, &["room", "archive", "vram"]);

    let out = ok(&home, &["room", "unarchive", "vram"]);
    assert!(out.contains("room 'vram' unarchived"), "{out}");
    assert!(home.join("rooms/vram").is_dir());
    assert!(!home.join("rooms/.archive/vram").exists());
    let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(home.join("rooms/vram/meta.json")).unwrap()).unwrap();
    assert_eq!(meta["members"], serde_json::json!(["p1", "p2"]));
    assert_eq!(meta["decision"]["note"], "12G each");
    assert_eq!(msgs(&home, "vram"), vec!["0001.json"]);
    // p1's own-say cursor rode the rename intact.
    assert_eq!(last_read(&home, "vram", "p1"), 1);

    // p2 never read: unread survived the round trip, and the
    // pre-archive message is still there to read.
    let out = ok(&home, &["room", "ls", "--as", "p2", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["rooms"][0]["unread_msgs"], 1, "{out}");
    let out = ok(&home, &["room", "read", "vram", "--as", "p2"]);
    assert!(out.contains("cap 12G?"), "pre-archive message readable after restore: {out}");
    std::fs::remove_dir_all(&home).ok();
}

/// The live/archive namespace invariant and rm's span: `create`
/// refuses an archived namesake naming `unarchive`/`rm`;
/// `unarchive` refuses a live collision (hand-crafted, since the
/// CLI keeps the namespaces disjoint); `rm` prefers the live
/// ladder and still reaches an exact archived name.
#[test]
fn namespace_invariant_and_rm_spanning_archive() {
    let home = tmpdir("ns-invariant");
    ok(&home, &["room", "create", "vram", "--members", "p1"]);
    ok(&home, &["room", "archive", "vram"]);

    // create refuses the archived namesake, naming both exits.
    let e = err(&home, &["room", "create", "vram", "--members", "p1"]);
    assert!(e.contains("an archived room 'vram' exists") && e.contains("unarchive") && e.contains("rm"), "{e}");

    // Hand-craft the collision the invariant forbids: a live room
    // and an archived room of the same name.
    ok(&home, &["room", "create", "other", "--members", "p1"]);
    std::fs::rename(home.join("rooms/other"), home.join("rooms/vram")).unwrap();
    let e = err(&home, &["room", "unarchive", "vram"]);
    assert!(e.contains("a live room 'vram' already exists"), "{e}");
    assert!(home.join("rooms/vram").is_dir() && home.join("rooms/.archive/vram").is_dir(), "the collision stays untouched");

    // rm prefers the live ladder: drops the live room, archive intact.
    let out = ok(&home, &["room", "rm", "vram"]);
    assert!(out.contains("room 'vram' removed") && !out.contains("archived"), "{out}");
    assert!(home.join("rooms/.archive/vram").is_dir());

    // Now the exact archived name is the only hit rm gets.
    let out = ok(&home, &["room", "rm", "vram"]);
    assert!(out.contains("archived room 'vram' removed"), "{out}");
    assert!(!home.join("rooms/.archive/vram").exists());

    // Cold storage is exact-only: a missing name — even a prefix of
    // an archived room — misses, and the error notes where archived
    // rooms list.
    ok(&home, &["room", "create", "gpu-bus", "--members", "p1"]);
    ok(&home, &["room", "archive", "gpu-bus"]);
    let e = err(&home, &["room", "unarchive", "gpu"]);
    assert!(e.contains("no archived room") && e.contains("room ls"), "{e}");
    let e = err(&home, &["room", "rm", "gpu"]);
    assert!(e.contains("no room"), "{e}");
    std::fs::remove_dir_all(&home).ok();
}
