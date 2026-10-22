//! Dotted key names must not alias a prefix-neighbor's public key.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static SEQ: AtomicUsize = AtomicUsize::new(0);

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("shunt-key-lifecycle-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
        for dir in ["registry/clients", "registry/keys", "mock-bin", ".ssh", "external"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        let ssh = root.join("mock-bin/ssh");
        std::fs::write(&ssh, "#!/bin/sh\nfor arg do last=$arg; done\ncase \"$last\" in\n  *'grep -vF'*) cat > \"$HOME/revoked-blob\"; exec sh -c \"$last\" < \"$HOME/revoked-blob\" ;;\nesac\nexit 0\n").unwrap();
        std::fs::set_permissions(ssh, std::fs::Permissions::from_mode(0o700)).unwrap();
        let keygen = root.join("mock-bin/ssh-keygen");
        std::fs::write(&keygen, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(keygen, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self { root }
    }

    fn pair(&self, relative: &str, blob: &str) -> PathBuf {
        let key = self.root.join(relative);
        std::fs::write(&key, format!("private {blob}")).unwrap();
        std::fs::write(format!("{}.pub", key.display()), format!("ssh-ed25519 {blob} original-comment\n")).unwrap();
        key
    }

    fn row(&self, name: &str, key: &Path) {
        let text = format!("name = {name:?}\nqroot = ''\ndeploy_root = ''\n[ssh]\nhost = 'example.invalid'\nuser = 'test'\nkey = {:?}\n", key.display().to_string());
        std::fs::write(self.root.join(format!("registry/clients/{name}.toml")), text).unwrap();
    }

    fn run(&self, args: &[&str]) {
        let output = Command::new(env!("CARGO_BIN_EXE_shunt")).args(args).env("SHUNT_HOME", self.root.join("registry")).env("SHUNT_ROOT", self.root.join("queue")).env("SHUNT_STATE", self.root.join("state")).env("HOME", &self.root).env("PATH", format!("{}:/usr/bin:/bin", self.root.join("mock-bin").display())).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }

    fn read(&self, relative: &str) -> String {
        std::fs::read_to_string(self.root.join(relative)).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.root).unwrap();
    }
}

#[test]
fn dotted_rename_moves_only_managed_pair() {
    let f = Fixture::new();
    let key = f.pair("registry/keys/gpu.one", "TARGET");
    f.pair("registry/keys/gpu", "PREFIX");
    f.pair("registry/keys/renamed", "DEST_PREFIX");
    f.row("gpu.one", &key);
    f.run(&["client", "rename", "gpu.one", "renamed.two"]);
    assert_eq!(f.read("registry/keys/renamed.two.pub"), "ssh-ed25519 TARGET original-comment\n");
    assert_eq!(f.read("registry/keys/renamed.two"), "private TARGET");
    assert!(!key.exists());
    assert!(!f.root.join("registry/keys/gpu.one.pub").exists());
    assert_eq!(f.read("registry/keys/gpu.pub"), "ssh-ed25519 PREFIX original-comment\n");
    assert_eq!(f.read("registry/keys/renamed.pub"), "ssh-ed25519 DEST_PREFIX original-comment\n");
    assert!(f.read("registry/clients/renamed.two.toml").contains("keys/renamed.two"));
}

#[test]
fn dotted_remove_revokes_target_and_preserves_prefix_pair() {
    let f = Fixture::new();
    let key = f.pair("registry/keys/gpu.one", "TARGET");
    f.pair("registry/keys/gpu", "PREFIX");
    f.row("gpu.one", &key);
    std::fs::write(f.root.join(".ssh/authorized_keys"), "ssh-ed25519 TARGET original-comment\nssh-ed25519 PREFIX neighbor\n").unwrap();
    f.run(&["client", "rm", "gpu.one"]);
    assert_eq!(f.read("revoked-blob"), "TARGET");
    assert_eq!(f.read(".ssh/authorized_keys"), "ssh-ed25519 PREFIX neighbor\n");
    assert!(!key.exists());
    assert!(!f.root.join("registry/keys/gpu.one.pub").exists());
    assert_eq!(f.read("registry/keys/gpu"), "private PREFIX");
    assert_eq!(f.read("registry/keys/gpu.pub"), "ssh-ed25519 PREFIX original-comment\n");
}

#[test]
fn external_dotted_key_is_revoked_but_never_moved_or_deleted() {
    let f = Fixture::new();
    let key = f.pair("external/id.custom", "EXTERNAL");
    f.pair("external/id", "PREFIX");
    f.row("gpu.one", &key);
    std::fs::write(f.root.join(".ssh/authorized_keys"), "ssh-ed25519 EXTERNAL original-comment\nssh-ed25519 PREFIX neighbor\n").unwrap();
    f.run(&["client", "rename", "gpu.one", "gpu.two"]);
    assert!(f.read("registry/clients/gpu.two.toml").contains(&key.display().to_string()));
    f.run(&["client", "rm", "gpu.two"]);
    assert_eq!(f.read("revoked-blob"), "EXTERNAL");
    assert_eq!(f.read(".ssh/authorized_keys"), "ssh-ed25519 PREFIX neighbor\n");
    assert_eq!(f.read("external/id.custom"), "private EXTERNAL");
    assert_eq!(f.read("external/id.custom.pub"), "ssh-ed25519 EXTERNAL original-comment\n");
    assert_eq!(f.read("external/id"), "private PREFIX");
    assert_eq!(f.read("external/id.pub"), "ssh-ed25519 PREFIX original-comment\n");
}
