//! Coordination rooms (design.md §17): server-local group chats
//! under `$SHUNT_HOME/rooms/`. Each case gets a tmp SHUNT_HOME;
//! concurrent-say coverage spawns real `shunt` processes so the
//! `.seq.lock` serialization is exercised for real.

mod common;

use common::bin;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("shunt-room-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn run(home: &Path, args: &[&str], extra: &[(&str, &str)]) -> Output {
    let mut c = Command::new(bin());
    c.args(args).env("SHUNT_HOME", home);
    for (k, v) in extra {
        c.env(k, v);
    }
    c.output().expect("shunt room run failed")
}

fn run_stdin(home: &Path, args: &[&str], input: &str) -> Output {
    use std::io::Write;
    let mut c = Command::new(bin()).args(args).env("SHUNT_HOME", home).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("spawn shunt room");
    c.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    c.wait_with_output().expect("shunt room run failed")
}

fn ok(home: &Path, args: &[&str]) -> String {
    let o = run(home, args, &[]);
    assert!(o.status.success(), "shunt {args:?} failed: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(home: &Path, args: &[&str]) -> String {
    let o = run(home, args, &[]);
    assert!(!o.status.success(), "shunt {args:?} unexpectedly ok: {}", String::from_utf8_lossy(&o.stdout));
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn msgs(home: &Path, room: &str) -> Vec<String> {
    let d = home.join("rooms").join(room).join("msgs");
    let mut v: Vec<String> = std::fs::read_dir(&d).map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| !n.starts_with('.')).collect()).unwrap_or_default();
    v.sort();
    v
}

fn archived(home: &Path, room: &str) -> Vec<String> {
    let d = home.join("rooms").join(room).join("msgs").join(".archive");
    let mut v: Vec<String> = std::fs::read_dir(&d).map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
    v.sort();
    v
}

fn last_read(home: &Path, room: &str, label: &str) -> u64 {
    let p = home.join("rooms").join(room).join("readers").join(format!("{label}.json"));
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
    v["last_read_n"].as_u64().unwrap()
}

fn write_client(home: &Path, name: &str, host: &str, port: u16) {
    let d = home.join("clients");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join(format!("{name}.toml")), format!("name = \"{name}\"\nqroot = \"/root/q\"\ndeploy_root = \"/root\"\n[ssh]\nhost = \"{host}\"\nport = {port}\nuser = \"root\"\n")).unwrap();
}

fn write_project(home: &Path, name: &str, binds: &[&str]) {
    let d = home.join("projects");
    std::fs::create_dir_all(&d).unwrap();
    let mut t = format!("name = \"{name}\"\n");
    for b in binds {
        t.push_str(&format!("[[bind]]\nclient = \"{b}\"\nworkdir = \"/w/{name}\"\n"));
    }
    std::fs::write(d.join(format!("{name}.toml")), t).unwrap();
}

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

/// A FIFO holds the actual CLI read after its directory snapshot but before EOF.
/// Concurrent say/read calls can then finish without timing-dependent races.
fn paused_read(home: &Path) -> (std::process::Child, std::fs::File, Vec<u8>) {
    use std::os::unix::fs::OpenOptionsExt;
    use std::time::{Duration, Instant};

    let path = home.join("rooms/r/msgs/0001.json");
    let body = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    assert!(Command::new("mkfifo").arg(&path).status().unwrap().success());
    let mut child = Command::new(bin()).args(["room", "read", "r", "--as", "p2", "--json"]).env("SHUNT_HOME", home).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match std::fs::OpenOptions::new().write(true).custom_flags(libc::O_NONBLOCK).open(&path) {
            Ok(writer) => return (child, writer, body),
            Err(e) if e.raw_os_error() == Some(libc::ENXIO) && Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            Err(e) => {
                child.kill().ok();
                child.wait().ok();
                panic!("read did not open FIFO: {e}");
            }
        }
    }
}

#[test]
fn read_does_not_acknowledge_concurrent_say() {
    use std::io::Write;
    let home = tmpdir("read-snapshot");
    ok(&home, &["room", "create", "r", "--members", "p1,p2"]);
    ok(&home, &["room", "say", "r", "--body", "first", "--as", "p1"]);
    let (reader, mut writer, body) = paused_read(&home);
    ok(&home, &["room", "say", "r", "--body", "unseen", "--as", "p1"]);
    writer.write_all(&body).unwrap();
    drop(writer);
    let output = reader.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(v["messages"].as_array().unwrap().len(), 1);
    assert_eq!(last_read(&home, "r", "p2"), 1, "unseen concurrent message must remain unread");
    // Replace the FIFO so the next real read can consume the live stream.
    std::fs::remove_file(home.join("rooms/r/msgs/0001.json")).unwrap();
    std::fs::write(home.join("rooms/r/msgs/0001.json"), body).unwrap();
    let out = ok(&home, &["room", "read", "r", "--as", "p2", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["messages"].as_array().unwrap().len(), 1);
    assert!(out.contains("unseen"));
    std::fs::remove_dir_all(home).unwrap();
}

#[test]
fn older_read_cannot_regress_shared_label_cursor() {
    use std::io::Write;
    let home = tmpdir("read-monotonic");
    ok(&home, &["room", "create", "r", "--members", "p1,p2"]);
    ok(&home, &["room", "say", "r", "--body", "first", "--as", "p1"]);
    let (reader, mut writer, body) = paused_read(&home);
    // The old reader retains the FIFO inode while a newer reader sees both files.
    let path = home.join("rooms/r/msgs/0001.json");
    std::fs::remove_file(&path).unwrap();
    std::fs::write(path, &body).unwrap();
    ok(&home, &["room", "say", "r", "--body", "second", "--as", "p1"]);
    ok(&home, &["room", "read", "r", "--as", "p2", "--json"]);
    assert_eq!(last_read(&home, "r", "p2"), 2);
    writer.write_all(&body).unwrap();
    drop(writer);
    let output = reader.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(last_read(&home, "r", "p2"), 2);
    let out = ok(&home, &["room", "read", "r", "--as", "p2", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(v["messages"].as_array().unwrap().is_empty());
    std::fs::remove_dir_all(home).unwrap();
}

#[test]
fn read_acknowledgement_excludes_archived_and_unloaded_tail() {
    let home = tmpdir("read-unloaded");
    ok(&home, &["room", "create", "r", "--members", "p1,p2"]);
    for body in ["archived", "loaded", "corrupt"] {
        ok(&home, &["room", "say", "r", "--body", body, "--as", "p1"]);
    }
    let dir = home.join("rooms/r/msgs");
    std::fs::create_dir(dir.join(".archive")).unwrap();
    std::fs::rename(dir.join("0001.json"), dir.join(".archive/0001.json")).unwrap();
    let tail = std::fs::read(dir.join("0003.json")).unwrap();
    std::fs::write(dir.join("0003.json"), "bad JSON").unwrap();
    let output = run(&home, &["room", "read", "r", "--as", "p2", "--all", "--json"], &[]);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("skipping unreadable message"));
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(v["messages"].as_array().unwrap().len(), 1);
    assert_eq!(last_read(&home, "r", "p2"), 2);
    std::fs::write(dir.join("0003.json"), tail).unwrap();
    let out = ok(&home, &["room", "read", "r", "--as", "p2", "--json"]);
    assert!(out.contains("corrupt"));
    assert_eq!(last_read(&home, "r", "p2"), 3);
    // A corrupt hole below a loaded message is crossed by the high-water mark.
    std::fs::write(dir.join("0002.json"), "bad JSON").unwrap();
    ok(&home, &["room", "read", "r", "--as", "fresh", "--json"]);
    assert_eq!(last_read(&home, "r", "fresh"), 3);
    std::fs::remove_file(dir.join("0003.json")).unwrap();
    // Nothing loadable must neither advance a fresh cursor nor regress an old one.
    ok(&home, &["room", "read", "r", "--as", "empty", "--all", "--json"]);
    assert_eq!(last_read(&home, "r", "empty"), 0);
    ok(&home, &["room", "read", "r", "--as", "p2", "--all", "--json"]);
    assert_eq!(last_read(&home, "r", "p2"), 3);
    std::fs::remove_dir_all(home).unwrap();
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

/// `room compact` plan picks the byte-weighted split — a fat first
/// message pulls the cut earlier than a count-based 80% would —
/// and is a pure audit read: no archive, no mark, no mutation.
#[test]
fn compact_plan_splits_by_bytes_and_mutates_nothing() {
    let home = tmpdir("cplan");
    let env = [("SHUNT_ROOM_COMPACT_MIN", "4")];
    ok(&home, &["room", "create", "r", "--members", "p1,p2"]);
    // 50+10+10+10 = 80 bytes; ratio .8 targets 64 → cum 50/60/70/80,
    // |60-64| smallest → split after message 2. Count-based would
    // land on message 4.
    for b in ["x".repeat(50), "x".repeat(10), "x".repeat(10), "x".repeat(10)] {
        ok(&home, &["room", "say", "r", "--body", &b, "--as", "p1"]);
    }
    let o = run(&home, &["room", "compact", "r", "--as", "p2", "--json"], &env);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["through"], 2, "{v}");
    assert_eq!(v["msg_count"], 2);
    assert_eq!(v["total_bytes"], 60);
    let msgs_v = v["messages"].as_array().unwrap();
    assert_eq!(msgs_v.len(), 2);
    assert_eq!(msgs_v[0]["n"], 1);
    assert_eq!(msgs_v[1]["n"], 2);
    assert!(msgs_v[0]["summary_through"].is_null(), "packet msgs carry the field for agent detection: {v}");

    // A smaller ratio cuts earlier.
    let o = run(&home, &["room", "compact", "r", "--as", "p2", "--ratio", "0.4", "--json"], &env);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["through"], 1, "{v}");
    assert_eq!(v["msg_count"], 1);

    // Pure audit read: no .archive, no cursor for p2, no new msgs.
    assert!(!home.join("rooms/r/msgs/.archive").exists(), "plan must not archive");
    assert!(!home.join("rooms/r/readers/p2.json").exists(), "plan must not mark");
    assert_eq!(msgs(&home, "r").len(), 4);
    std::fs::remove_dir_all(&home).ok();
}

/// Below compact_min the plan refuses and names the manual escape;
/// a forced --through still lands under the floor.
#[test]
fn compact_plan_refuses_below_floor() {
    let home = tmpdir("cfloor");
    let env = [("SHUNT_ROOM_COMPACT_MIN", "5")];
    ok(&home, &["room", "create", "r", "--members", "p1"]);
    for i in 1..=3 {
        ok(&home, &["room", "say", "r", "--body", &format!("m{i}"), "--as", "p1"]);
    }
    let o = run(&home, &["room", "compact", "r"], &env);
    assert!(!o.status.success(), "plan under the floor must refuse: {}", String::from_utf8_lossy(&o.stdout));
    let e = String::from_utf8_lossy(&o.stderr);
    assert!(e.contains("non-summary live messages") && e.contains("--through"), "{e}");
    // The forced commit is the documented escape.
    let o = run(&home, &["room", "compact", "r", "--through", "2", "--body", "d", "--as", "p1"], &env);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    std::fs::remove_dir_all(&home).ok();
}

/// Commit archives the covered originals, mints the digest
/// in-stream, lands the meta ratchet, and marks the caller — while
/// staying one ordinary message to `read --all`.
#[test]
fn compact_commit_archives_mints_ratchet_marks() {
    let home = tmpdir("ccommit");
    ok(&home, &["room", "create", "r", "--members", "p1,p2"]);
    for i in 1..=5 {
        ok(&home, &["room", "say", "r", "--body", &format!("m{i}"), "--as", "p1"]);
    }
    // Rewind p2's cursor so the covered-range warning names it.
    ok(&home, &["room", "read", "r", "--as", "p2"]);
    std::fs::write(home.join("rooms/r/readers/p2.json"), "{\"last_read_n\":1}").unwrap();

    let o = run(&home, &["room", "compact", "r", "--through", "3", "--body", "digest of 1-3", "--as", "p1", "--json"], &[]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let e = String::from_utf8_lossy(&o.stderr);
    assert!(e.contains("covered range: p2"), "the warning names the behind reader: {e}");
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["through"], 3, "{v}");
    assert_eq!(v["summary_n"], 6);
    assert_eq!(v["member"], true);

    assert_eq!(msgs(&home, "r"), vec!["0004.json", "0005.json", "0006.json"]);
    assert_eq!(archived(&home, "r"), vec!["0001.json", "0002.json", "0003.json"]);
    let s: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(home.join("rooms/r/msgs/0006.json")).unwrap()).unwrap();
    assert_eq!(s["summary_through"], 3);
    assert_eq!(s["body_md"], "digest of 1-3");
    assert_eq!(s["member"], true);
    let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(home.join("rooms/r/meta.json")).unwrap()).unwrap();
    assert_eq!(meta["compacted_through"], 3);
    // Commit marks the caller through the live max, like say.
    assert_eq!(last_read(&home, "r", "p1"), 6);

    // The digest is an ordinary stream entry — `read --all` shows it
    // in place, tagged in human output.
    let out = ok(&home, &["room", "read", "r", "--as", "p2", "--all"]);
    assert!(out.contains("(summary→0003)") && out.contains("digest of 1-3"), "{out}");
    std::fs::remove_dir_all(&home).ok();
}

/// A --through at or below the ratchet is the one coverage refusal;
/// a retry against a digest that committed but never landed meta
/// returns its n — no duplicate mint, first body retained.
#[test]
fn compact_rejects_regression_and_recommits_idempotently() {
    let home = tmpdir("cidem");
    ok(&home, &["room", "create", "r", "--members", "p1"]);
    for i in 1..=5 {
        ok(&home, &["room", "say", "r", "--body", &format!("m{i}"), "--as", "p1"]);
    }
    ok(&home, &["room", "compact", "r", "--through", "3", "--body", "first digest", "--as", "p1"]);
    for t in ["2", "3"] {
        let e = err(&home, &["room", "compact", "r", "--through", t, "--body", "x", "--as", "p1"]);
        assert!(e.contains("already covered"), "{e}");
    }
    // Simulate the post-write failure the idempotency check exists
    // for: the digest is live but the ratchet never landed.
    let meta_path = home.join("rooms/r/meta.json");
    let mut meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&meta_path).unwrap()).unwrap();
    meta.as_object_mut().unwrap().remove("compacted_through");
    std::fs::write(&meta_path, serde_json::to_string(&meta).unwrap()).unwrap();
    let o = run(&home, &["room", "compact", "r", "--through", "3", "--body", "rewritten digest", "--as", "p1", "--json"], &[]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&o.stdout).unwrap()["summary_n"], 6);
    assert_eq!(msgs(&home, "r"), vec!["0004.json", "0005.json", "0006.json"], "no duplicate mint");
    assert_eq!(archived(&home, "r"), vec!["0001.json", "0002.json", "0003.json"], "no double move");
    let s: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(home.join("rooms/r/msgs/0006.json")).unwrap()).unwrap();
    assert_eq!(s["body_md"], "first digest", "the committed digest survives the retry");
    let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&meta_path).unwrap()).unwrap();
    assert_eq!(meta["compacted_through"], 3, "the retry lands the ratchet");
    std::fs::remove_dir_all(&home).ok();
}

/// Prior digests are exempt from the compact move — only originals
/// ≤N archive; a second compact ratchets forward.
#[test]
fn compact_move_exempts_prior_summaries() {
    let home = tmpdir("cexempt");
    ok(&home, &["room", "create", "r", "--members", "p1"]);
    for i in 1..=5 {
        ok(&home, &["room", "say", "r", "--body", &format!("m{i}"), "--as", "p1"]);
    }
    ok(&home, &["room", "compact", "r", "--through", "3", "--body", "d1", "--as", "p1"]);
    for i in 7..=9 {
        ok(&home, &["room", "say", "r", "--body", &format!("m{i}"), "--as", "p1"]);
    }
    // through=9 covers the live head: warning (b) fires but the
    // commit still lands.
    let o = run(&home, &["room", "compact", "r", "--through", "9", "--body", "d2", "--as", "p1", "--json"], &[]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(String::from_utf8_lossy(&o.stderr).contains("newest live message"));
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&o.stdout).unwrap()["summary_n"], 10);
    // Covered originals 4,5,7,8,9 archived; the old digest stays.
    assert_eq!(msgs(&home, "r"), vec!["0006.json", "0010.json"]);
    assert_eq!(archived(&home, "r"), vec!["0001.json", "0002.json", "0003.json", "0004.json", "0005.json", "0007.json", "0008.json", "0009.json"]);
    let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(home.join("rooms/r/meta.json")).unwrap()).unwrap();
    assert_eq!(meta["compacted_through"], 9);
    std::fs::remove_dir_all(&home).ok();
}

/// A non-member commit warns like say (open trust, member:false);
/// through ≥ the live head warns; behind readers are named. None of
/// it gates.
#[test]
fn compact_commit_warns_without_gating() {
    let home = tmpdir("cwarn");
    ok(&home, &["room", "create", "r", "--members", "p1,p2"]);
    for i in 1..=3 {
        ok(&home, &["room", "say", "r", "--body", &format!("m{i}"), "--as", "p1"]);
    }
    ok(&home, &["room", "read", "r", "--as", "p2"]);
    std::fs::write(home.join("rooms/r/readers/p2.json"), "{\"last_read_n\":1}").unwrap();
    let o = run(&home, &["room", "compact", "r", "--through", "3", "--body", "d", "--as", "intruder", "--json"], &[]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let e = String::from_utf8_lossy(&o.stderr);
    assert!(e.contains("not a member") && e.contains("roster: p1, p2"), "{e}");
    assert!(e.contains("newest live message"), "{e}");
    assert!(e.contains("covered range: p2 —"), "only the behind reader is named: {e}");
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["member"], false);
    assert_eq!(msgs(&home, "r"), vec!["0004.json"]);
    std::fs::remove_dir_all(&home).ok();
}

/// `--from/--to` reads span live + archive (inclusive, either bound
/// open) and never mark — a pure audit read.
#[test]
fn read_range_spans_archive_and_never_marks() {
    let home = tmpdir("crange");
    ok(&home, &["room", "create", "r", "--members", "p1,p2"]);
    for i in 1..=5 {
        ok(&home, &["room", "say", "r", "--body", &format!("m{i}"), "--as", "p1"]);
    }
    ok(&home, &["room", "compact", "r", "--through", "3", "--body", "digest", "--as", "p1"]);

    let out = ok(&home, &["room", "read", "r", "--as", "p2", "--from", "1", "--to", "4", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let ns: Vec<u64> = v["messages"].as_array().unwrap().iter().map(|m| m["n"].as_u64().unwrap()).collect();
    assert_eq!(ns, vec![1, 2, 3, 4], "archived 1-3 + live 4: {out}");
    assert!(!home.join("rooms/r/readers/p2.json").exists(), "range read must not mark");

    // Either bound may be omitted (open range).
    let out = ok(&home, &["room", "read", "r", "--as", "p2", "--from", "5", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let ns: Vec<u64> = v["messages"].as_array().unwrap().iter().map(|m| m["n"].as_u64().unwrap()).collect();
    assert_eq!(ns, vec![5, 6], "{out}");
    assert_eq!(v["messages"][1]["summary_through"], 3);
    let out = ok(&home, &["room", "read", "r", "--as", "p2", "--to", "2"]);
    assert!(out.contains("m1") && out.contains("m2") && !out.contains("m3"), "{out}");

    // Human output tags digest headers; still no marking.
    let out = ok(&home, &["room", "read", "r", "--as", "p2", "--from", "6"]);
    assert!(out.contains("(summary→0003)"), "{out}");
    assert!(!home.join("rooms/r/readers/p2.json").exists());
    std::fs::remove_dir_all(&home).ok();
}

/// `read --digest` is the compacted catch-up: the newest summary
/// plus the raw live tail past its coverage, marked like --all.
/// With no compaction yet it notes so and returns the full stream.
#[test]
fn read_digest_returns_summary_and_tail() {
    let home = tmpdir("cdigest");
    ok(&home, &["room", "create", "r", "--members", "p1,p2"]);
    for i in 1..=5 {
        ok(&home, &["room", "say", "r", "--body", &format!("m{i}"), "--as", "p1"]);
    }
    // No compaction yet: a stderr note, the full live stream.
    let o = run(&home, &["room", "read", "r", "--as", "p2", "--digest"], &[]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(String::from_utf8_lossy(&o.stderr).contains("no compaction yet"));
    assert!(String::from_utf8_lossy(&o.stdout).contains("m1"));
    assert_eq!(last_read(&home, "r", "p2"), 5);

    ok(&home, &["room", "compact", "r", "--through", "3", "--body", "digest of 1-3", "--as", "p1"]);
    ok(&home, &["room", "say", "r", "--body", "tail", "--as", "p1"]);
    // Live is 4,5,6(sum→3),7 — the digest plus every n > 3.
    let out = ok(&home, &["room", "read", "r", "--as", "p2", "--digest", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let ns: Vec<u64> = v["messages"].as_array().unwrap().iter().map(|m| m["n"].as_u64().unwrap()).collect();
    assert_eq!(ns, vec![4, 5, 6, 7], "{out}");
    assert_eq!(v["messages"][2]["summary_through"], 3);
    assert_eq!(v["messages"][2]["body_md"], "digest of 1-3");
    assert_eq!(last_read(&home, "r", "p2"), 7, "digest marks through the returned max");
    std::fs::remove_dir_all(&home).ok();
}

/// An unread read surfaces the digest like any other message — the
/// originals are gone from live, the summary arrives as catch-up.
#[test]
fn unread_read_surfaces_the_summary() {
    let home = tmpdir("cunread");
    ok(&home, &["room", "create", "r", "--members", "p1,p2"]);
    for i in 1..=5 {
        ok(&home, &["room", "say", "r", "--body", &format!("m{i}"), "--as", "p1"]);
    }
    ok(&home, &["room", "read", "r", "--as", "p2"]);
    assert_eq!(last_read(&home, "r", "p2"), 5);
    ok(&home, &["room", "compact", "r", "--through", "3", "--body", "d", "--as", "p1"]);
    let out = ok(&home, &["room", "read", "r", "--as", "p2", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let msgs_v = v["messages"].as_array().unwrap();
    assert_eq!(msgs_v.len(), 1, "{out}");
    assert_eq!(msgs_v[0]["n"], 6);
    assert_eq!(msgs_v[0]["summary_through"], 3);
    assert_eq!(last_read(&home, "r", "p2"), 6);
    std::fs::remove_dir_all(&home).ok();
}

/// Summaries are exempt only from the compact MOVE — the live cap
/// still drains them like any message, and `read --digest` then
/// falls back to the archive for the newest one.
#[test]
fn cap_drain_takes_summaries_and_digest_falls_back() {
    let home = tmpdir("ccap");
    let env = [("SHUNT_ROOM_KEEP", "3")];
    ok(&home, &["room", "create", "r", "--members", "p1,p2"]);
    for i in 1..=3 {
        let o = run(&home, &["room", "say", "r", "--body", &format!("m{i}"), "--as", "p1"], &env);
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    }
    ok(&home, &["room", "compact", "r", "--through", "2", "--body", "d", "--as", "p1"]); // live = 3, 4(sum→2)
    for i in 5..=7 {
        let o = run(&home, &["room", "say", "r", "--body", &format!("m{i}"), "--as", "p1"], &env);
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    }
    // Cap=3 drained 3 then the summary 4 into .archive.
    assert_eq!(msgs(&home, "r"), vec!["0005.json", "0006.json", "0007.json"]);
    assert!(archived(&home, "r").contains(&"0004.json".to_string()), "the cap drains summaries too: {:?}", archived(&home, "r"));
    // Digest view: no live summary → the archived 0004 heads it,
    // then the raw tail n > 2.
    let out = ok(&home, &["room", "read", "r", "--as", "p2", "--digest", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let ns: Vec<u64> = v["messages"].as_array().unwrap().iter().map(|m| m["n"].as_u64().unwrap()).collect();
    assert_eq!(ns, vec![4, 5, 6, 7], "{out}");
    assert_eq!(v["messages"][0]["summary_through"], 2);
    std::fs::remove_dir_all(&home).ok();
}

/// The phase-dispatch validation: --through with no body source is
/// the commit error; a body source without --through names the
/// plan step; --ratio out of range refuses.
#[test]
fn compact_arg_validation() {
    let home = tmpdir("cargs");
    ok(&home, &["room", "create", "r", "--members", "p1"]);
    ok(&home, &["room", "say", "r", "--body", "m1", "--as", "p1"]);
    let e = err(&home, &["room", "compact", "r", "--through", "1"]);
    assert!(e.contains("digest body"), "{e}");
    let e = err(&home, &["room", "compact", "r", "--body", "d"]);
    assert!(e.contains("--through") && e.contains("split point"), "{e}");
    let e = err(&home, &["room", "compact", "r", "--ratio", "1.5"]);
    assert!(e.contains("between 0 and 1"), "{e}");
    let e = err(&home, &["room", "compact", "r", "--through", "0", "--body", "d"]);
    assert!(e.contains("--through 0"), "{e}");
    std::fs::remove_dir_all(&home).ok();
}

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
