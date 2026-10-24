//! Room-test fixtures shared by the `rooms*` targets: a tmp
//! SHUNT_HOME per case, the `run`/`ok`/`err` process wrappers,
//! msgs/readers dir readers, and client/project registry writers.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use super::bin;

pub fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("shunt-room-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

pub fn run(home: &Path, args: &[&str], extra: &[(&str, &str)]) -> Output {
    let mut c = Command::new(bin());
    c.args(args).env("SHUNT_HOME", home);
    for (k, v) in extra {
        c.env(k, v);
    }
    c.output().expect("shunt room run failed")
}

pub fn run_stdin(home: &Path, args: &[&str], input: &str) -> Output {
    use std::io::Write;
    let mut c = Command::new(bin()).args(args).env("SHUNT_HOME", home).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("spawn shunt room");
    c.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    c.wait_with_output().expect("shunt room run failed")
}

pub fn ok(home: &Path, args: &[&str]) -> String {
    let o = run(home, args, &[]);
    assert!(o.status.success(), "shunt {args:?} failed: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).into_owned()
}

pub fn err(home: &Path, args: &[&str]) -> String {
    let o = run(home, args, &[]);
    assert!(!o.status.success(), "shunt {args:?} unexpectedly ok: {}", String::from_utf8_lossy(&o.stdout));
    String::from_utf8_lossy(&o.stderr).into_owned()
}

pub fn msgs(home: &Path, room: &str) -> Vec<String> {
    let d = home.join("rooms").join(room).join("msgs");
    let mut v: Vec<String> = std::fs::read_dir(&d).map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| !n.starts_with('.')).collect()).unwrap_or_default();
    v.sort();
    v
}

pub fn archived(home: &Path, room: &str) -> Vec<String> {
    let d = home.join("rooms").join(room).join("msgs").join(".archive");
    let mut v: Vec<String> = std::fs::read_dir(&d).map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
    v.sort();
    v
}

pub fn last_read(home: &Path, room: &str, label: &str) -> u64 {
    let p = home.join("rooms").join(room).join("readers").join(format!("{label}.json"));
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
    v["last_read_n"].as_u64().unwrap()
}

pub fn write_client(home: &Path, name: &str, host: &str, port: u16) {
    let d = home.join("clients");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join(format!("{name}.toml")), format!("name = \"{name}\"\nqroot = \"/root/q\"\ndeploy_root = \"/root\"\n[ssh]\nhost = \"{host}\"\nport = {port}\nuser = \"root\"\n")).unwrap();
}

pub fn write_project(home: &Path, name: &str, binds: &[&str]) {
    let d = home.join("projects");
    std::fs::create_dir_all(&d).unwrap();
    let mut t = format!("name = \"{name}\"\n");
    for b in binds {
        t.push_str(&format!("[[bind]]\nclient = \"{b}\"\nworkdir = \"/w/{name}\"\n"));
    }
    std::fs::write(d.join(format!("{name}.toml")), t).unwrap();
}
