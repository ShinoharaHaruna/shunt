//! Host-key edits must not alias any other client's store or backup name.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

static SEQ: AtomicUsize = AtomicUsize::new(0);
const ORIGINAL: &str = "example.invalid OLD\n[new.example.invalid]:4022 NEW\nkeep.invalid KEEP\n";
const NEIGHBORS: [&str; 5] = ["gpu", "gpu.old", "gpu.one.old", "gpu.one.old.old", "gpu.one.old.old.old"];

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("shunt-hostkey-hygiene-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
        for dir in ["registry/clients", "registry/known_hosts", "mock-bin", ".ssh"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        let f = Self { root };
        f.script("ssh", "#!/bin/sh\nexit 0\n");
        // Model OpenSSH's backup overwrite and partial output even on failure.
        f.script(
            "ssh-keygen",
            r#"#!/bin/sh
set -eu
[ "$1" = -R ] && [ "$3" = -f ]
printf '%s\n' "$4" >> "$HOME/keygen-paths"
[ "$(stat -c %a "$(dirname "$4")")" = 700 ]
[ "$(stat -c %a "$4")" = 600 ]
cp "$4" "$4.old"
printf 'temporary key material\n' > "$4.temporary"
printf 'partial edit\n' > "$4"
[ "${KEYGEN_FAIL:-0}" = 0 ] || exit 1
awk -v spec="$2" '$1 != spec' "$4.old" > "$4"
# OpenSSH may replace the file with one created under the process umask.
chmod 644 "$4"
"#,
        );
        std::fs::write(f.root.join(".ssh/known_hosts"), "USER STORE\n").unwrap();
        for name in NEIGHBORS {
            f.store(name, &format!("NEIGHBOR {name}\n"));
        }
        f.store("gpu.one", ORIGINAL);
        f.row("gpu.one", "example.invalid", 22);
        f
    }

    fn script(&self, name: &str, body: &str) {
        let path = self.root.join("mock-bin").join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn store(&self, name: &str, body: &str) {
        let path = self.root.join("registry/known_hosts").join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn row(&self, name: &str, host: &str, port: u16) {
        std::fs::write(self.root.join(format!("registry/clients/{name}.toml")), format!("name = {name:?}\nqroot = {:?}\ndeploy_root = {:?}\n[ssh]\nhost = {host:?}\nport = {port}\nuser = 'test'\n", self.root.join("queue").display().to_string(), self.root.display().to_string())).unwrap();
    }

    fn run(&self, args: &[&str], fail: bool) -> Output {
        Command::new(env!("CARGO_BIN_EXE_shunt")).args(args).env("SHUNT_HOME", self.root.join("registry")).env("SHUNT_ROOT", self.root.join("queue")).env("SHUNT_STATE", self.root.join("state")).env("HOME", &self.root).env("KEYGEN_FAIL", if fail { "1" } else { "0" }).env("PATH", format!("{}:/usr/bin:/bin", self.root.join("mock-bin").display())).output().unwrap()
    }

    fn ok(&self, args: &[&str]) {
        let out = self.run(args, false);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }

    fn read(&self, relative: &str) -> String {
        std::fs::read_to_string(self.root.join(relative)).unwrap_or_else(|err| panic!("read {relative}: {err}"))
    }

    fn assert_isolated(&self, target: &str, removed: bool) {
        for name in NEIGHBORS.into_iter().filter(|name| *name != target) {
            assert_eq!(self.read(&format!("registry/known_hosts/{name}")), format!("NEIGHBOR {name}\n"), "neighbor {name} changed");
        }
        assert_eq!(self.read(".ssh/known_hosts"), "USER STORE\n");
        let mut expected: Vec<_> = NEIGHBORS.into_iter().map(str::to_string).collect();
        expected.push("gpu.one".into());
        if removed {
            expected.retain(|name| name != target);
        }
        expected.sort();
        let mut actual: Vec<_> = std::fs::read_dir(self.root.join("registry/known_hosts")).unwrap().map(|entry| entry.unwrap().file_name().into_string().unwrap()).collect();
        actual.sort();
        assert_eq!(actual, expected, "staging/backup residue or missing store");
        if !removed {
            let path = self.root.join("registry/known_hosts").join(target);
            assert_eq!(std::fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    fn assert_staged(&self) {
        let paths = self.read("keygen-paths");
        let mut seen = std::collections::BTreeSet::new();
        for path in paths.lines().map(PathBuf::from) {
            assert_ne!(path.parent().unwrap(), self.root.join("registry/known_hosts"));
            assert!(path.starts_with(self.root.join("registry/known_hosts")), "staging must stay on the store filesystem");
            assert!(!path.parent().unwrap().exists(), "staging directory leaked");
            assert!(seen.insert(path), "staging path reused");
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.root).unwrap();
    }
}

#[test]
fn dotted_hostkey_preserves_neighbor_stores_and_cleans_backups() {
    let f = Fixture::new();
    f.ok(&["client", "hostkey", "gpu.one"]);
    assert_eq!(f.read("registry/known_hosts/gpu.one"), "[new.example.invalid]:4022 NEW\nkeep.invalid KEEP\n");
    f.assert_isolated("gpu.one", false);
    f.ok(&["client", "hostkey", "gpu.one"]);
    f.assert_isolated("gpu.one", false);
    f.assert_staged();
}

#[test]
fn old_suffix_client_is_not_a_backup_namespace() {
    let f = Fixture::new();
    f.store("gpu.one.old", ORIGINAL);
    f.row("gpu.one.old", "example.invalid", 22);
    f.ok(&["client", "hostkey", "gpu.one.old"]);
    assert_eq!(f.read("registry/known_hosts/gpu.one.old"), "[new.example.invalid]:4022 NEW\nkeep.invalid KEEP\n");
    assert_eq!(f.read("registry/known_hosts/gpu.one"), ORIGINAL);
    f.assert_isolated("gpu.one.old", false);
    f.assert_staged();
}

#[test]
fn failed_hostkey_preserves_source_neighbors_and_cleans_partial_work() {
    let f = Fixture::new();
    let out = f.run(&["client", "hostkey", "gpu.one"], true);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("ssh-keygen -R"));
    assert_eq!(f.read("registry/known_hosts/gpu.one"), ORIGINAL);
    f.assert_isolated("gpu.one", false);
    f.assert_staged();
}

#[test]
fn dotted_update_drops_both_endpoints_without_touching_neighbors() {
    let f = Fixture::new();
    f.ok(&["client", "update", "gpu.one", "--host", "new.example.invalid", "--port", "4022"]);
    assert_eq!(f.read("registry/known_hosts/gpu.one"), "keep.invalid KEEP\n");
    f.assert_isolated("gpu.one", false);
    f.assert_staged();
}

#[test]
fn real_keygen_isolates_success_and_invalid_input() {
    let f = Fixture::new();
    let key_path = f.root.join("host-key");
    let out = Command::new("/usr/bin/ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-f"]).arg(&key_path).env("HOME", &f.root).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    std::fs::remove_file(f.root.join("mock-bin/ssh-keygen")).unwrap();
    let key = f.read("host-key.pub").split_whitespace().take(2).collect::<Vec<_>>().join(" ");
    let keep = format!("keep.invalid {key}\n");
    f.store("gpu.one", &format!("example.invalid {key}\n[new.example.invalid]:4022 {key}\n{keep}"));
    f.ok(&["client", "hostkey", "gpu.one"]);
    assert_eq!(f.read("registry/known_hosts/gpu.one"), format!("[new.example.invalid]:4022 {key}\n{keep}"));
    f.assert_isolated("gpu.one", false);
    // A missing host is a no-op, including OpenSSH builds that skip the backup.
    f.ok(&["client", "hostkey", "gpu.one"]);
    f.assert_isolated("gpu.one", false);
    f.ok(&["client", "update", "gpu.one", "--host", "new.example.invalid", "--port", "4022"]);
    assert_eq!(f.read("registry/known_hosts/gpu.one"), keep);
    f.assert_isolated("gpu.one", false);

    let invalid = format!("{keep}invalid known_hosts line\n");
    f.store("gpu.one", &invalid);
    let out = f.run(&["client", "hostkey", "gpu.one"], false);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("ssh-keygen -R"));
    assert_eq!(f.read("registry/known_hosts/gpu.one"), invalid);
    f.assert_isolated("gpu.one", false);
}

#[test]
fn dotted_remove_preserves_neighbors_and_leaves_no_backup() {
    let f = Fixture::new();
    f.ok(&["client", "rm", "gpu.one"]);
    assert!(!f.root.join("registry/clients/gpu.one.toml").exists());
    f.assert_isolated("gpu.one", true);
    f.assert_staged();
}
