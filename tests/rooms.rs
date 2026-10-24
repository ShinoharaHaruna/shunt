//! Coordination rooms (design.md §17): server-local group chats
//! under `$SHUNT_HOME/rooms/`. Each case gets a tmp SHUNT_HOME;
//! concurrent-say coverage spawns real `shunt` processes so the
//! `.seq.lock` serialization is exercised for real.
//!
//! This target covers the core ops — create/say/read/ls/decide,
//! the live cap, suggestions, and status. Split alongside:
//! `rooms_membership` (§17.3 labels), `rooms_cursors` (read-cursor
//! races), `rooms_compact` (§17 compaction), `rooms_archive`
//! (room-level archive).

mod common;

use common::bin;
use common::room::*;
use std::process::Command;

/// Concurrent `say` mints unique message numbers under `.seq.lock`
/// (§11): N parallel posts land as N distinct NNNN.json files.
#[test]
fn concurrent_say_mints_unique_numbers() {
    let home = tmpdir("race");
    ok(&home, &["room", "create", "r", "--members", "p1,p2"]);

    let mut kids = Vec::new();
    for i in 0..8 {
        let h = home.clone();
        kids.push(std::thread::spawn(move || {
            let o = Command::new(bin()).args(["room", "say", "r", "--body", &format!("msg-{i}"), "--as", "p1"]).env("SHUNT_HOME", &h).output().unwrap();
            assert!(o.status.success(), "say {i} failed: {}", String::from_utf8_lossy(&o.stderr));
        }));
    }
    for k in kids {
        k.join().unwrap();
    }
    let names = msgs(&home, "r");
    assert_eq!(names.len(), 8, "expected 8 messages, got {names:?}");
    assert_eq!(names, vec!["0001.json", "0002.json", "0003.json", "0004.json", "0005.json", "0006.json", "0007.json", "0008.json"]);
    std::fs::remove_dir_all(&home).ok();
}

/// `read` prints only messages past the caller's mark and advances
/// it; `ls`/`status` report unread without marking (§17.5).
#[test]
fn read_marks_and_unread_counts() {
    let home = tmpdir("marks");
    ok(&home, &["room", "create", "r", "--members", "p1,p2"]);
    for i in 1..=3 {
        ok(&home, &["room", "say", "r", "--body", &format!("m{i}"), "--as", "p1"]);
    }
    // p1's own posts are read for p1 (a say marks its own message).
    assert_eq!(last_read(&home, "r", "p1"), 3);

    // p2 has never engaged: ls reports 3 unread WITHOUT marking.
    let out = ok(&home, &["room", "ls", "--as", "p2", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["rooms"][0]["unread_msgs"], 3, "{out}");
    assert!(!home.join("rooms/r/readers/p2.json").exists(), "ls must not mark");

    // read returns all 3 for p2, then marks.
    let out = ok(&home, &["room", "read", "r", "--as", "p2", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["messages"].as_array().unwrap().len(), 3, "{out}");
    assert_eq!(last_read(&home, "r", "p2"), 3);

    // One more post: p2 sees exactly it (unread-only default).
    ok(&home, &["room", "say", "r", "--body", "m4", "--as", "p1"]);
    let out = ok(&home, &["room", "read", "r", "--as", "p2"]);
    assert!(out.contains("m4") && !out.contains("m1"), "{out}");
    // --all prints the full stream, still marks.
    let out = ok(&home, &["room", "read", "r", "--as", "p2", "--all"]);
    for m in ["m1", "m2", "m3", "m4"] {
        assert!(out.contains(m), "{out}");
    }
    std::fs::remove_dir_all(&home).ok();
}

/// `room ls` prints `msgs` (live total) beside `unread(<label>)` —
/// the column header names the resolved reader, so `--all` never
/// shows a bare total under an 'unread' claim (issue #10, §17.5).
#[test]
fn ls_distinguishes_msgs_and_attributes_unread() {
    let home = tmpdir("ls-cols");
    ok(&home, &["room", "create", "r1", "--members", "p1,p2"]);
    ok(&home, &["room", "create", "r2", "--members", "p3"]);
    for i in 1..=3 {
        ok(&home, &["room", "say", "r1", "--body", &format!("m{i}"), "--as", "p1"]);
    }
    ok(&home, &["room", "say", "r2", "--body", "x1", "--as", "p3"]);
    // p2 reads all of r1: its unread is 0 while the total stays 3.
    ok(&home, &["room", "read", "r1", "--as", "p2", "--all"]);

    // Member label: the row shows that label's unread, not a total,
    // and the header names the reader.
    let out = ok(&home, &["room", "ls", "--as", "p2"]);
    assert!(out.contains("unread(p2)"), "{out}");
    let r1: Vec<&str> = out.lines().find(|l| l.starts_with("r1")).unwrap().split_whitespace().collect();
    assert_eq!(r1[1], "3", "msgs is the live total: {out}");
    assert_eq!(r1[2], "0", "unread is p2's count, not the total: {out}");
    assert!(!out.lines().any(|l| l.starts_with("r2")), "member ls stays label-scoped: {out}");

    // --all with an explicit --as names that reader.
    let out = ok(&home, &["room", "ls", "--all", "--as", "p1"]);
    assert!(out.contains("unread(p1)"), "{out}");

    // --all without --as attributes the column to `admin` — never a
    // bare 'unread' header. Admin never read r1, so its count equals
    // the total — the header, not the number, says whose it is.
    let out = ok(&home, &["room", "ls", "--all"]);
    assert!(out.contains("unread(admin)"), "{out}");
    assert!(!out.contains("unread  "), "unattributed unread column must be gone: {out}");
    let r1: Vec<&str> = out.lines().find(|l| l.starts_with("r1")).unwrap().split_whitespace().collect();
    assert_eq!(r1[1], "3", "{out}");
    assert_eq!(r1[2], "3", "admin's unread equals the total and is labeled admin's: {out}");
    let r2: Vec<&str> = out.lines().find(|l| l.starts_with("r2")).unwrap().split_whitespace().collect();
    assert_eq!(r2[1], "1");
    assert_eq!(r2[2], "1");

    // JSON keeps the attributed unread per room plus the resolved
    // `as` label — same semantics, machine-readable.
    let out = ok(&home, &["room", "ls", "--all", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["as"], "admin", "{out}");
    let r1 = v["rooms"].as_array().unwrap().iter().find(|r| r["name"] == "r1").unwrap();
    assert_eq!(r1["msgs"], 3);
    assert_eq!(r1["unread_msgs"], 3);
    std::fs::remove_dir_all(&home).ok();
}

/// `decide` overwrites meta.decision (latest wins) and marks the
/// caller's cursor (§17.2/§17.5).
#[test]
fn decide_overwrites_and_marks() {
    let home = tmpdir("decide");
    ok(&home, &["room", "create", "r", "--members", "p1,p2"]);
    ok(&home, &["room", "say", "r", "--body", "proposal", "--as", "p1"]);
    ok(&home, &["room", "decide", "r", "--note", "cap 12G", "--as", "p2"]);
    ok(&home, &["room", "decide", "r", "--note", "cap 8G", "--as", "p1"]);
    let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(home.join("rooms/r/meta.json")).unwrap()).unwrap();
    assert_eq!(meta["decision"]["note"], "cap 8G");
    assert_eq!(meta["decision"]["author"], "p1");
    // decide marks: p1's cursor covers the stream even though
    // decide posts no message.
    assert_eq!(last_read(&home, "r", "p1"), 1);
    std::fs::remove_dir_all(&home).ok();
}

/// Past the live cap, oldest NNNN.json move to msgs/.archive/
/// (§17.4) — dot-prefixed, invisible to read/ls.
#[test]
fn cap_archives_oldest_first() {
    let home = tmpdir("cap");
    let env = [("SHUNT_ROOM_KEEP", "4")];
    ok(&home, &["room", "create", "r", "--members", "p1"]);
    for i in 1..=6 {
        let o = run(&home, &["room", "say", "r", "--body", &format!("m{i}"), "--as", "p1"], &env);
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    }
    assert_eq!(msgs(&home, "r"), vec!["0003.json", "0004.json", "0005.json", "0006.json"]);
    let mut arch: Vec<String> = std::fs::read_dir(home.join("rooms/r/msgs/.archive")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    arch.sort();
    assert_eq!(arch, vec!["0001.json", "0002.json"]);
    // Archived messages are invisible to `read --all`.
    let out = ok(&home, &["room", "read", "r", "--as", "p2", "--all"]);
    assert!(!out.contains("m1") && out.contains("m6"), "{out}");
    std::fs::remove_dir_all(&home).ok();
}

/// `room ls` suggests groups of projects whose bindings share one
/// ssh endpoint (and suppresses the suggestion once a room covers
/// them, §17.4). Drifted members are flagged stale.
#[test]
fn ls_suggests_shared_endpoint_groups() {
    let home = tmpdir("suggest");
    write_client(&home, "box-a", "203.0.113.9", 12345);
    write_client(&home, "box-b", "203.0.113.9", 12345); // same box, second row
    write_client(&home, "box-c", "198.51.100.7", 22);
    write_project(&home, "p1", &["box-a"]);
    write_project(&home, "p2", &["box-b"]);
    write_project(&home, "p3", &["box-a"]);
    write_project(&home, "solo", &["box-c"]);

    let out = ok(&home, &["room", "ls", "--as", "p1", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let sg = v["suggested"].as_array().unwrap();
    assert_eq!(sg.len(), 1, "{out}");
    assert_eq!(sg[0]["endpoint"], "203.0.113.9:12345");
    assert_eq!(sg[0]["projects"], serde_json::json!(["p1", "p2", "p3"]));
    // A lone project on its own endpoint suggests nothing.

    // Creating a covering room suppresses the suggestion and the
    // room lists for its member label.
    ok(&home, &["room", "create", "vram", "--members", "p1,p2,p3"]);
    let out = ok(&home, &["room", "ls", "--as", "p1", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["suggested"].as_array().unwrap().len(), 0, "{out}");
    assert_eq!(v["rooms"].as_array().unwrap().len(), 1, "{out}");

    // Drift: unbind p2 -> it shares no group -> stale, while p1/p3
    // still share the endpoint and stay live (§17.4).
    let p2 = home.join("projects/p2.toml");
    std::fs::write(&p2, "name = \"p2\"\n").unwrap();
    let out = ok(&home, &["room", "members", "vram", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["room"]["stale"], serde_json::json!(["p2"]), "{out}");
    std::fs::remove_dir_all(&home).ok();
}

/// Body over 16 KiB is rejected before anything is written (§17.1).
#[test]
fn body_cap_rejects_oversize() {
    let home = tmpdir("bigbody");
    ok(&home, &["room", "create", "r", "--members", "p1"]);
    let big = "x".repeat(16 * 1024 + 1);
    let e = err(&home, &["room", "say", "r", "--body", &big, "--as", "p1"]);
    assert!(e.contains("cap"), "{e}");
    assert!(msgs(&home, "r").is_empty(), "oversize must not write");

    let exact = "x".repeat(16 * 1024);
    ok(&home, &["room", "say", "r", "--body", &exact, "--as", "p1"]);
    assert_eq!(msgs(&home, "r"), vec!["0001.json"]);

    // `-`/stdin is a body source too (§14.1 precedence).
    let o = run_stdin(&home, &["room", "say", "r", "-", "--as", "p1"], "from stdin\n");
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(msgs(&home, "r").len(), 2);
    std::fs::remove_dir_all(&home).ok();
}

/// `status --json` carries the rooms block for --as (§17.5) —
/// report-only, so it must not mark the reader.
#[test]
fn status_carries_rooms_block() {
    let home = tmpdir("status");
    ok(&home, &["room", "create", "vram", "--members", "p1,p2"]);
    ok(&home, &["room", "say", "vram", "--body", "cap?", "--as", "p1"]);
    ok(&home, &["room", "decide", "vram", "--note", "12G each", "--as", "p1"]);

    let out = ok(&home, &["status", "--json", "--as", "p2"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let rooms = v["rooms"].as_array().unwrap();
    assert_eq!(rooms.len(), 1, "{out}");
    assert_eq!(rooms[0]["name"], "vram");
    assert_eq!(rooms[0]["unread_msgs"], 1);
    assert_eq!(rooms[0]["decision"]["note"], "12G each");
    assert!(!home.join("rooms/vram/readers/p2.json").exists(), "status must not mark");
    std::fs::remove_dir_all(&home).ok();
}

/// Unique-prefix resolution, readers/ eviction cap, and corrupt
/// .seq remediation (§17.2/§17.4/§3.2).
#[test]
fn prefix_eviction_and_seq_faults() {
    let home = tmpdir("misc");
    ok(&home, &["room", "create", "gpu-vram", "--members", "p1,p2"]);
    // Unique prefix resolves; ambiguous prefix lists matches.
    ok(&home, &["room", "say", "gpu", "--body", "hi", "--as", "p1"]);
    let e = err(&home, &["room", "read", "nope", "--as", "p1"]);
    assert!(e.contains("no room"), "{e}");

    // readers/ capped at SHUNT_ROOM_READERS_MAX, oldest-mtime out.
    let env = [("SHUNT_ROOM_READERS_MAX", "2")];
    for l in ["la", "lb", "lc"] {
        let o = run(&home, &["room", "say", "gpu-vram", "--body", "x", "--as", l], &env);
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        std::thread::sleep(std::time::Duration::from_millis(5)); // distinct mtimes
    }
    let mut readers: Vec<String> = std::fs::read_dir(home.join("rooms/gpu-vram/readers")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| !n.starts_with('.')).collect();
    readers.sort();
    assert_eq!(readers, vec!["lb.json", "lc.json"], "{readers:?}");

    // Corrupt .seq fails with remediation; removing it rebuilds
    // from the high-water mark.
    std::fs::write(home.join("rooms/gpu-vram/.seq"), "garbage").unwrap();
    let e = err(&home, &["room", "say", "gpu-vram", "--body", "x", "--as", "p1"]);
    assert!(e.contains("corrupt") && e.contains("high-water"), "{e}");
    std::fs::remove_file(home.join("rooms/gpu-vram/.seq")).unwrap();
    ok(&home, &["room", "say", "gpu-vram", "--body", "x", "--as", "p1"]);
    // Live msgs: p1's first post + la/lb/lc + this one = 5.
    assert_eq!(msgs(&home, "gpu-vram").last().map(String::as_str), Some("0005.json"));
    std::fs::remove_dir_all(&home).ok();
}

/// The unit contract rejects an already claimed directory deterministically;
/// real processes additionally exercise the CLI with distinct competing rosters.
#[test]
fn concurrent_create_has_one_owner_and_preserves_existing_paths() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::sync::{Arc, Barrier};

    let home = tmpdir("create-exclusive");
    let barrier = Arc::new(Barrier::new(8));
    let kids: Vec<_> = (0..8)
        .map(|i| {
            let home = home.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let member = format!("project-{i}");
                barrier.wait();
                let output = run(&home, &["room", "create", "shared", "--members", &member], &[]);
                (member, output)
            })
        })
        .collect();
    let results: Vec<_> = kids.into_iter().map(|k| k.join().unwrap()).collect();
    let winners: Vec<_> = results.iter().filter(|(_, o)| o.status.success()).collect();
    assert_eq!(winners.len(), 1, "{results:?}");
    let dir = home.join("rooms/shared");
    let meta: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("meta.json")).unwrap()).unwrap();
    assert_eq!(meta["members"], serde_json::json!([winners[0].0]));
    for path in [&dir, &dir.join("msgs"), &dir.join("readers")] {
        assert_eq!(std::fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o700);
    }
    assert_eq!(std::fs::metadata(dir.join("meta.json")).unwrap().permissions().mode() & 0o777, 0o600);
    let before = std::fs::read(dir.join("meta.json")).unwrap();
    err(&home, &["room", "create", "shared", "--members", "other"]);
    assert_eq!(std::fs::read(dir.join("meta.json")).unwrap(), before);

    let empty = home.join("rooms/empty");
    std::fs::create_dir(&empty).unwrap();
    std::fs::set_permissions(&empty, std::fs::Permissions::from_mode(0o755)).unwrap();
    err(&home, &["room", "create", "empty", "--members", "other"]);
    assert_eq!(std::fs::read_dir(&empty).unwrap().count(), 0);
    assert_eq!(std::fs::metadata(&empty).unwrap().permissions().mode() & 0o777, 0o755);
    std::fs::write(home.join("rooms/file"), "untouched").unwrap();
    err(&home, &["room", "create", "file", "--members", "other"]);
    symlink(home.join("missing-target"), home.join("rooms/link")).unwrap();
    err(&home, &["room", "create", "link", "--members", "other"]);
    assert!(!home.join("missing-target").exists());
    assert_eq!(std::fs::read_to_string(home.join("rooms/file")).unwrap(), "untouched");
    std::fs::remove_dir_all(home).unwrap();
}
