//! Rescue runs in child test processes so mock tool environments stay isolated.

use super::*;
use std::os::unix::fs::PermissionsExt;
use std::process::Output;
use std::sync::atomic::{AtomicUsize, Ordering};

static SEQ: AtomicUsize = AtomicUsize::new(0);

struct Fixture {
    root: PathBuf,
}

fn script(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

impl Fixture {
    fn new(tag: &str, sha: &str, corrupt: bool) -> Self {
        let root = std::env::temp_dir().join(format!("shunt-rescue-tags-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::SeqCst)));
        for dir in ["bin", "assets", "tmp", "home", "queue", "state"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        script(&root.join("local"), "#!/bin/sh\nprintf 'shunt 99.0.0 (abcdef0 2026-01-01)\\n'\n");
        let name = format!("shunt-{tag}-{MUSL_TARGET}");
        std::fs::create_dir(root.join(&name)).unwrap();
        script(&root.join(&name).join("shunt"), &format!("#!/bin/sh\nprintf 'shunt 99.0.0 ({sha} 2026-01-01)\\n'\n"));
        let asset = format!("{name}.tar.gz");
        let out = Command::new("/usr/bin/tar").arg("-czf").arg(root.join("assets").join(&asset)).arg("-C").arg(&root).arg(&name).output().unwrap();
        assert!(out.status.success(), "{out:?}");
        let out = Command::new("/usr/bin/sha256sum").arg(&asset).current_dir(root.join("assets")).output().unwrap();
        assert!(out.status.success());
        let manifest = if corrupt { format!("{}  {asset}\n", "0".repeat(64)) } else { String::from_utf8(out.stdout).unwrap() };
        std::fs::write(root.join("assets").join(format!("SHA256SUMS-{MUSL_TARGET}")), manifest).unwrap();
        script(
            &root.join("bin/gh"),
            r#"#!/bin/sh
printf 'gh %s\n' "$*" >> "$MOCK_LOG"
[ "$MOCK_TRANSPORT" = gh ] || exit 1
if [ "$1 $2" = 'release view' ]; then
    [ "$3" = "$MOCK_TAG" ] || [ "$3" = --repo ] || exit 1
    printf '%s\n' "$MOCK_TAG"
    exit 0
fi
[ "$1 $2 $3" = "release download $MOCK_TAG" ] || exit 1
while [ $# -gt 0 ]; do
case "$1" in -p) file=$2; shift;; -D) dir=$2; shift;; esac
shift
done
/bin/cp "$MOCK_ASSETS/$file" "$dir/$file"
"#,
        );
        script(
            &root.join("bin/curl"),
            r#"#!/bin/sh
printf 'curl %s\n' "$*" >> "$MOCK_LOG"
while [ $# -gt 0 ]; do
case "$1" in -o) dest=$2; shift;; https://*) url=$1;; esac
shift
done
case "$url" in
https://api.github.com/repos/fixture/offline/releases/latest) printf '{"tag_name":"%s"}\n' "$MOCK_TAG";;
https://api.github.com/repos/fixture/offline/releases/tags/*)
    if [ "$MOCK_QUERY_ERROR" = yes ]; then printf '{}\n500';
    elif [ "${url##*/}" = "$MOCK_TAG" ]; then printf '{"tag_name":"%s"}\n200' "$MOCK_TAG";
    else printf '{}\n404'; fi;;
https://github.com/fixture/offline/releases/download/"$MOCK_TAG"/*)
    [ "$MOCK_TRANSPORT" = curl ] || exit 1
    /bin/cp "$MOCK_ASSETS/${url##*/}" "$dest";;
*) exit 1;;
esac
"#,
        );
        for tool in ["sha256sum", "tar", "gzip"] {
            std::os::unix::fs::symlink(format!("/usr/bin/{tool}"), root.join("bin").join(tool)).unwrap();
        }
        Self { root }
    }

    fn rescue(&self, tag: &str, transport: &str, expected: &str, query_error: bool) -> Output {
        Command::new(std::env::current_exe().unwrap()).args(["--exact", "server::client_ops::release::tag_tests::rescue_child", "--nocapture"]).env("RESCUE_FIXTURE", &self.root).env("RESCUE_EXPECTED", expected).env("PATH", self.root.join("bin")).env("TMPDIR", self.root.join("tmp")).env("SHUNT_HOME", self.root.join("home")).env("SHUNT_ROOT", self.root.join("queue")).env("SHUNT_STATE", self.root.join("state")).env("SHUNT_RELEASE_REPO", "fixture/offline").env("MOCK_TAG", tag).env("MOCK_TRANSPORT", transport).env("MOCK_ASSETS", self.root.join("assets")).env("MOCK_LOG", self.root.join("requests")).env("MOCK_QUERY_ERROR", if query_error { "yes" } else { "no" }).output().unwrap()
    }

    fn requests(&self) -> String {
        std::fs::read_to_string(self.root.join("requests")).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn rescue_child() {
    let Ok(root) = std::env::var("RESCUE_FIXTURE") else { return };
    let expected = std::env::var("RESCUE_EXPECTED").unwrap();
    let result = if expected == "legacy" {
        assert_eq!(latest_version().unwrap(), "99.0.0");
        fetch_asset("99.0.0", MUSL_TARGET)
    } else {
        rescue_musl(&Path::new(&root).join("local"))
    };
    if expected == "ok" || expected == "legacy" {
        let bin = result.unwrap();
        let tag = std::env::var("MOCK_TAG").unwrap();
        assert_eq!(bin.path().parent().unwrap().file_name().unwrap(), format!("shunt-{tag}-{MUSL_TARGET}").as_str());
        assert!(local_version(bin.path()).unwrap().contains("abcdef0"));
    } else {
        let err = result.err().expect("invalid rescue accepted");
        assert!(format!("{err:#}").contains(&expected), "{err:#}");
    }
}

#[test]
fn rescue_resolves_both_tag_styles_before_fetch() {
    for tag in ["v99.0.0", "99.0.0"] {
        for transport in ["gh", "curl"] {
            let f = Fixture::new(tag, "abcdef0", false);
            let out = f.rescue(tag, transport, "ok", false);
            assert!(out.status.success(), "{out:?}");
            let requests = f.requests();
            assert!(requests.contains(&format!("release download {tag}")), "{requests}");
            assert!(requests.contains(&format!("shunt-{tag}-{MUSL_TARGET}.tar.gz")), "{requests}");
            if tag == "99.0.0" {
                assert!(requests.contains("/releases/tags/v99.0.0"), "{requests}");
                assert!(!requests.contains("release download v99.0.0"), "{requests}");
            }
        }
    }
}

#[test]
fn rescue_does_not_retry_another_tag_after_integrity_or_identity_failure() {
    for tag in ["v99.0.0", "99.0.0"] {
        for transport in ["gh", "curl"] {
            for (sha, corrupt, expected) in [("abcdef0", true, "checksum mismatch"), ("badc0de", false, "refusing to ship different code")] {
                let f = Fixture::new(tag, sha, corrupt);
                let out = f.rescue(tag, transport, expected, false);
                assert!(out.status.success(), "{out:?}");
                let requests = f.requests();
                let other = if tag.starts_with('v') { "99.0.0" } else { "v99.0.0" };
                assert!(!requests.contains(&format!("release download {other}")), "{requests}");
                assert!(!requests.contains(&format!("/download/{other}/")), "{requests}");
            }
        }
    }
}

#[test]
fn rescue_does_not_treat_query_failure_as_missing_tag() {
    let f = Fixture::new("99.0.0", "abcdef0", false);
    let out = f.rescue("99.0.0", "curl", "500", true);
    assert!(out.status.success(), "{out:?}");
    let requests = f.requests();
    assert!(!requests.contains("/releases/tags/99.0.0"), "{requests}");
    assert!(!requests.contains("release download"), "{requests}");
}

#[test]
fn legacy_release_helpers_keep_normalized_version_and_prefixed_assets() {
    for transport in ["gh", "curl"] {
        let f = Fixture::new("v99.0.0", "abcdef0", false);
        let out = f.rescue("v99.0.0", transport, "legacy", false);
        assert!(out.status.success(), "{out:?}");
    }
}
