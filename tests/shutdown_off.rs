//! Disarming must be retry-safe without hiding flag deletion failures.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

static SEQ: AtomicUsize = AtomicUsize::new(0);

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("shunt-shutdown-off-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("shutdown_cmd"), "true\n").unwrap();
        Self { root }
    }

    fn run(&self, action: &str, json: bool) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_shunt"));
        cmd.args(["shutdown", action, "--root"]).arg(&self.root).env("SHUNT_HOME", self.root.join("home")).env("SHUNT_ROOT", &self.root).env("SHUNT_STATE", self.root.join("boxstate")).env("SHUTDOWN_CMD", "true");
        if json {
            cmd.arg("--json");
        }
        cmd.output().unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.root).unwrap();
    }
}

#[test]
fn shutdown_off_is_idempotent() {
    let f = Fixture::new();
    let flag = f.root.join("shutdown_on_idle");
    for json in [false, true] {
        for state in ["missing", "armed", "retry"] {
            if state == "armed" {
                std::fs::write(&flag, b"").unwrap();
            }
            let output = f.run("off", json);
            assert!(output.status.success(), "{state}, json={json}: {}", String::from_utf8_lossy(&output.stderr));
            assert!(!flag.exists());
            if json {
                let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
                assert_eq!(value["protocol"], 1);
                assert_eq!(value["ok"], true);
                assert_eq!(value["armed"], false);
            } else {
                assert!(String::from_utf8_lossy(&output.stdout).contains("disarmed:"));
            }
        }
    }
}

#[test]
fn shutdown_off_reports_deletion_failure() {
    let f = Fixture::new();
    let flag = f.root.join("shutdown_on_idle");
    // A directory makes remove_file fail even for privileged test runners.
    std::fs::create_dir(&flag).unwrap();
    let output = f.run("off", false);
    assert!(flag.is_dir());
    assert!(!output.status.success(), "false success: {}", String::from_utf8_lossy(&output.stdout));
    assert!(output.stdout.is_empty(), "must not print a disarmed acknowledgement");
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains(flag.to_str().unwrap()), "{error}");
    assert!(error.contains("permissions") && error.contains("retry") && error.contains("shunt shutdown off"), "{error}");
}

#[test]
fn shutdown_off_json_reports_deletion_failure() {
    let f = Fixture::new();
    let flag = f.root.join("shutdown_on_idle");
    std::fs::create_dir(&flag).unwrap();
    let output = f.run("off", true);
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(flag.is_dir());
    assert!(!output.status.success(), "false success: {value}");
    assert_eq!(value["protocol"], 1);
    assert_eq!(value["ok"], false);
    assert_ne!(value["armed"], false, "a failed removal cannot report disarmed");
    let error = value["error"].as_str().unwrap();
    assert!(error.contains(flag.to_str().unwrap()), "{error}");
    assert!(error.contains("permissions") && error.contains("retry") && error.contains("shunt shutdown off"), "{error}");

    let status = f.run("status", true);
    assert!(status.status.success());
    let value: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(value["armed"], true);
}
