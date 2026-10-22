//! Removal must validate both identities before executing commands or changing state.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

static SEQ: AtomicUsize = AtomicUsize::new(0);

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("shunt-remove-identity-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(root.join("registry/clients")).unwrap();
        std::fs::create_dir_all(root.join("queue/pending")).unwrap();
        std::fs::write(root.join("queue/marker"), "queue data").unwrap();
        std::fs::create_dir_all(root.join("mock-bin")).unwrap();
        for name in ["ssh", "ssh-keygen", "local-shunt"] {
            let path = root.join("mock-bin").join(name);
            std::fs::write(&path, "#!/bin/sh\nprintf '%s\\n' \"$0 $*\" >> \"$SHUNT_HOME/commands\"\nexit 0\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        for dir in ["cm", "state", "tunnels", "known_hosts", "keys"] {
            std::fs::create_dir_all(root.join("registry").join(dir)).unwrap();
        }
        for name in ["box", "victim"] {
            for (dir, ext) in [("cm", ".sock"), ("state", ".fail"), ("tunnels", ".port"), ("tunnels", ".log"), ("known_hosts", ""), ("keys", ""), ("keys", ".pub")] {
                std::fs::write(root.join("registry").join(dir).join(format!("{name}{ext}")), "sentinel").unwrap();
            }
        }
        std::fs::write(root.join("registry/server.toml"), "default_client = \"box\"\n").unwrap();
        Self { root }
    }

    fn row(&self, file: &str, inner: &str, local: bool, disabled: bool) {
        let text = format!("name = {inner:?}\nlocal = {local}\ndisabled = {disabled}\nlocal_bin = {:?}\nqroot = {:?}\ndeploy_root = ''\n[ssh]\nhost = 'mock.example'\nuser = 'test'\n", self.root.join("mock-bin/local-shunt").display().to_string(), self.root.join("queue").display().to_string());
        std::fs::write(self.root.join("registry").join(file), text).unwrap();
    }

    fn remove(&self, name: &str, purge: bool) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_shunt"));
        cmd.args(["client", "rm", name]).env("SHUNT_HOME", self.root.join("registry")).env("SHUNT_ROOT", self.root.join("queue")).env("SHUNT_STATE", self.root.join("boxstate")).env("HOME", &self.root).env("PATH", format!("{}:/usr/bin:/bin", self.root.join("mock-bin").display()));
        if purge {
            cmd.arg("--purge");
        }
        cmd.output().unwrap()
    }

    fn assert_rejected_unchanged(&self, name: &str, purge: bool) {
        let before = snapshot(&self.root);
        let output = self.remove(name, purge);
        assert_eq!(snapshot(&self.root), before, "removal of {name:?} (purge={purge}) changed state before validating identity");
        assert!(!output.status.success(), "invalid identity was accepted");
        let err = String::from_utf8_lossy(&output.stderr);
        assert!(err.contains("name"), "expected an identity error, got: {err}");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.root).unwrap();
    }
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    fn visit(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let data = if path.is_dir() { None } else { Some(std::fs::read(&path).unwrap()) };
            out.insert(path.strip_prefix(root).unwrap().to_owned(), data);
            if path.is_dir() {
                visit(root, &path, out);
            }
        }
    }
    let mut out = BTreeMap::new();
    visit(root, root, &mut out);
    out
}

#[test]
fn remove_rejects_requested_name_traversal_without_side_effects() {
    for purge in [false, true] {
        let f = Fixture::new();
        f.row("outside.toml", "outside", false, false);
        f.assert_rejected_unchanged("../outside", purge);
    }
}

#[test]
fn remove_rejects_forged_inner_names_without_side_effects() {
    for inner in ["victim", "localhost", "../../outside"] {
        for local in [false, true] {
            for purge in [false, true] {
                for disabled in [false, true] {
                    let f = Fixture::new();
                    f.row("clients/box.toml", inner, local, disabled);
                    f.assert_rejected_unchanged("box", purge);
                }
            }
        }
    }
}

#[test]
fn remove_accepts_valid_disabled_rows() {
    for purge in [false, true] {
        let f = Fixture::new();
        f.row("clients/box.toml", "box", true, true);
        let output = f.remove("box", purge);
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert!(!f.root.join("registry/clients/box.toml").exists());
        assert!(!f.root.join("registry/cm/box.sock").exists());
        assert!(f.root.join("registry/cm/victim.sock").exists());
        assert_eq!(f.root.join("queue/marker").exists(), !purge);
        assert!(std::fs::read_to_string(f.root.join("registry/commands")).unwrap().contains("stop"));
    }
}
