//! Coordination rooms (design.md §17.5): read-cursor semantics under
//! races and partial reads — a `read` acknowledges only what its
//! directory snapshot loaded, cursors never regress, and archived
//! or unreadable tails are excluded from the mark.

mod common;

use common::bin;
use common::room::*;
use std::path::Path;
use std::process::{Command, Stdio};

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
