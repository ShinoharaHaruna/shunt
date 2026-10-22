//! Release integrity gates use local assets and hermetic download commands.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::Ordering;

const VERSION: &str = "99.0.0";
const TARGETS: [&str; 2] = ["x86_64-unknown-linux-gnu", "x86_64-unknown-linux-musl"];
const EMPTY_SHA: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

struct Release {
    root: PathBuf,
}

fn script(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

impl Release {
    fn new(binary_version: &str) -> Self {
        let n = common::SEQ.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!("shunt-release-checksum-{}-{n}", std::process::id()));
        let fixture = Self { root };
        for dir in ["bin", "assets", "tmp", "home", "queue", "state"] {
            std::fs::create_dir_all(fixture.root.join(dir)).unwrap();
        }
        std::fs::copy(common::bin(), fixture.root.join("shunt")).unwrap();
        script(&fixture.root.join("bin/gh"), "#!/bin/sh\nif [ \"$1 $2\" = 'release view' ]; then printf 'v99.0.0\\n'; exit 0; fi\n[ \"$MOCK_TRANSPORT\" = gh ] || exit 1\nwhile [ $# -gt 0 ]; do\ncase \"$1\" in -p) file=$2; shift;; -D) dir=$2; shift;; esac\nshift\ndone\n/bin/cp \"$MOCK_ASSETS/$file\" \"$dir/$file\"\n");
        script(&fixture.root.join("bin/curl"), "#!/bin/sh\n[ \"$MOCK_TRANSPORT\" = curl ] || exit 1\nwhile [ $# -gt 0 ]; do\ncase \"$1\" in -o) dest=$2; shift;; https://*) file=${1##*/};; esac\nshift\ndone\n/bin/cp \"$MOCK_ASSETS/$file\" \"$dest\"\n");
        script(&fixture.root.join("bin/tar"), "#!/bin/sh\nprintf 'extract\\n' >> \"$TAR_MARKER\"\nexec /usr/bin/tar \"$@\"\n");
        for tool in ["sha256sum", "gzip"] {
            std::os::unix::fs::symlink(format!("/usr/bin/{tool}"), fixture.root.join("bin").join(tool)).unwrap();
        }
        for target in TARGETS {
            let name = format!("shunt-v{VERSION}-{target}");
            let payload = fixture.root.join(&name);
            std::fs::create_dir(&payload).unwrap();
            script(&payload.join("shunt"), &format!("#!/bin/sh\nprintf 'execute\\n' >> \"$EXEC_MARKER\"\nprintf 'shunt {binary_version} (abcdef0 2026-01-01)\\n'\n"));
            let out = Command::new("/usr/bin/tar").arg("-czf").arg(fixture.root.join("assets").join(format!("{name}.tar.gz"))).arg("-C").arg(&fixture.root).arg(&name).output().unwrap();
            assert!(out.status.success(), "{out:?}");
        }
        fixture
    }

    fn manifests(&self, make: impl Fn(&str, &str) -> String) {
        for target in TARGETS {
            let asset = format!("shunt-v{VERSION}-{target}.tar.gz");
            let out = Command::new("/usr/bin/sha256sum").arg(&asset).current_dir(self.root.join("assets")).output().unwrap();
            assert!(out.status.success());
            let generated = String::from_utf8(out.stdout).unwrap();
            let digest = generated.split_whitespace().next().unwrap();
            std::fs::write(self.root.join("assets").join(format!("SHA256SUMS-{target}")), make(digest, &asset)).unwrap();
        }
    }

    fn upgrade(&self, transport: &str) -> Output {
        Command::new(self.root.join("shunt")).arg("upgrade").env("PATH", self.root.join("bin")).env("TMPDIR", self.root.join("tmp")).env("SHUNT_HOME", self.root.join("home")).env("SHUNT_ROOT", self.root.join("queue")).env("SHUNT_STATE", self.root.join("state")).env("SHUNT_RELEASE_REPO", "fixture/offline").env("MOCK_ASSETS", self.root.join("assets")).env("MOCK_TRANSPORT", transport).env("TAR_MARKER", self.root.join("extracted")).env("EXEC_MARKER", self.root.join("executed")).output().unwrap()
    }

    fn rejected(&self, transport: &str) {
        let out = self.upgrade(transport);
        assert!(!self.root.join("extracted").exists(), "invalid release reached tar: {out:?}");
        assert!(!self.root.join("executed").exists(), "invalid release binary ran: {out:?}");
        assert!(!out.status.success(), "invalid release accepted: {out:?}");
        assert!(String::from_utf8_lossy(&out.stderr).contains("checksum"), "{out:?}");
        assert_eq!(std::fs::read(self.root.join("shunt")).unwrap(), std::fs::read(common::bin()).unwrap());
    }
}

impl Drop for Release {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn release_checksum_requires_expected_archive() {
    for transport in ["gh", "curl"] {
        let fixture = Release::new(VERSION);
        fixture.manifests(|_, _| format!("{EMPTY_SHA}  /dev/null\n"));
        fixture.rejected(transport);
    }
}

#[test]
fn release_checksum_rejects_wrong_digest() {
    for transport in ["gh", "curl"] {
        let fixture = Release::new(VERSION);
        fixture.manifests(|_, asset| format!("{}  {asset}\n", "0".repeat(64)));
        fixture.rejected(transport);
    }
}

#[test]
fn release_checksum_rejects_malformed_entries() {
    let fixture = Release::new(VERSION);
    for manifest in ["".to_string(), "not a checksum\n".to_string()] {
        fixture.manifests(|_, _| manifest.clone());
        fixture.rejected("gh");
    }
    for digest in ["a".repeat(63), "g".repeat(64)] {
        fixture.manifests(|_, asset| format!("{digest}  {asset}\n"));
        fixture.rejected("curl");
    }
    fixture.manifests(|digest, asset| format!("{digest}  {asset}\nmalformed\n"));
    fixture.rejected("gh");
}

#[test]
fn release_checksum_rejects_ambiguous_entries() {
    for transport in ["gh", "curl"] {
        let fixture = Release::new(VERSION);
        fixture.manifests(|digest, asset| format!("{digest}  {asset}\n{digest} *{asset}\n"));
        fixture.rejected(transport);
        fixture.manifests(|digest, asset| format!("{digest}  {asset}\n{}  {asset}\n", "0".repeat(64)));
        fixture.rejected(transport);
    }
}

#[test]
fn release_checksum_rejects_manifest_paths() {
    let fixture = Release::new(VERSION);
    for path in ["/dev/null", "../outside", "./archive", "nested/archive"] {
        fixture.manifests(|digest, asset| format!("{digest}  {asset}\n{EMPTY_SHA}  {path}\n"));
        fixture.rejected("gh");
    }
}

#[test]
fn release_checksum_accepts_generated_and_binary_formats() {
    for transport in ["gh", "curl"] {
        for mode in [" ", "*"] {
            let fixture = Release::new(VERSION);
            fixture.manifests(|digest, asset| format!("{digest} {mode}{asset}\n"));
            let out = fixture.upgrade(transport);
            assert!(out.status.success(), "{out:?}");
            assert!(fixture.root.join("extracted").exists());
            assert!(fixture.root.join("executed").exists());
            assert!(String::from_utf8_lossy(&out.stdout).contains("upgraded"));
        }
    }
}

#[test]
fn release_checksum_only_hashes_expected_file() {
    let fixture = Release::new(VERSION);
    fixture.manifests(|digest, asset| format!("{digest}  {asset}\n{EMPTY_SHA}  not-downloaded.tar.gz\n"));
    let out = fixture.upgrade("gh");
    assert!(out.status.success(), "{out:?}");
    assert!(fixture.root.join("executed").exists());
}

#[test]
fn release_checksum_preserves_upgrade_identity_check() {
    let fixture = Release::new("98.0.0");
    fixture.manifests(|digest, asset| format!("{digest}  {asset}\n"));
    let out = fixture.upgrade("gh");
    assert!(!out.status.success(), "{out:?}");
    assert!(fixture.root.join("extracted").exists());
    assert!(fixture.root.join("executed").exists());
    assert!(String::from_utf8_lossy(&out.stderr).contains("expected 99.0.0"), "{out:?}");
    assert_eq!(std::fs::read(fixture.root.join("shunt")).unwrap(), std::fs::read(common::bin()).unwrap());
}
