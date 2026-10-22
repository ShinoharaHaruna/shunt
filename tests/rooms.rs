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
