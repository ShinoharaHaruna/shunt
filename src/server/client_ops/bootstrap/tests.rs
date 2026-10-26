//! Bootstrap unit tests: the owner claim, the §15 classify ladder,
//! and the live-daemon handoff — all behind fake Transports.

use super::{bootstrap, bootstrap_queue, claim::CLAIM_SCRIPT};
use crate::server::client_ops::test_entry as base_entry;
use crate::server::registry::{self, tests::HOME_LOCK, ClientEntry};
use crate::server::transport::Transport;
use std::path::PathBuf;
use std::sync::Mutex;

/// Transport that must never be reached: the qroot guard fires
/// before the first wire op.
struct Panic;
impl Transport for Panic {
    fn exec(&self, _c: &ClientEntry, _a: &[String], _s: Option<&str>) -> anyhow::Result<String> {
        panic!("bootstrap ran a wire op with an empty qroot")
    }
    fn copy_to(&self, _c: &ClientEntry, _l: &std::path::Path, _r: &str) -> anyhow::Result<()> {
        panic!("bootstrap copied with an empty qroot")
    }
}

#[test]
fn bootstrap_rejects_empty_qroot_on_local_row() {
    // A custom `--local` row (not the built-in localhost, which
    // materializes a real qroot) with qroot="" would render
    // shunt_bin as `/bin/shunt` and stage the binary toward
    // SYSTEM /bin — reject at the guard, before any wire op.
    let _g = HOME_LOCK.lock().unwrap();
    let tmp = std::env::temp_dir().join(format!("shunt-test-boot-qroot-{}", std::process::id()));
    std::env::set_var("SHUNT_HOME", &tmp);
    let mut entry = super::super::test_entry();
    entry.name = "loctest".into();
    entry.local = true;
    entry.qroot = String::new();
    registry::save_client(&entry).unwrap();
    let err = bootstrap(&Panic, "loctest", None).unwrap_err();
    std::env::remove_var("SHUNT_HOME");
    std::fs::remove_dir_all(&tmp).ok();
    assert!(err.to_string().contains("qroot is empty"), "{err}");
}

/// What the staged `<bin>.new --version 2>&1` smoke answers —
/// the §15 classifier's only input.
enum Smoke {
    /// Bare version line → Compat::Ok.
    Clean,
    /// Version line plus loader noise → Compat::Warn. Kept free
    /// of any `GLIBC_x.y` literal: it would embed in this test
    /// binary and poison required_glibc(current_exe) — observed
    /// poisoning in the predecessor test.
    Warn,
    /// Exec fails → Compat::Error.
    Fail,
}

/// Canned answers per argv: `.shunt-owner` reports the fixture
/// claim; `status` reports liveness for the handoff branch;
/// `shunt.new --version` is the staged smoke (`smoke` field);
/// any other `--version` is the post-install confirm or local
/// drift print. `copy_to` is recorded so tests can assert what
/// staged, and `calls` records argv so tests can check mv/rm.
struct Stub {
    ldd: &'static str,
    /// Marker the .shunt-owner claim script answers.
    claim: &'static str,
    smoke: Smoke,
    /// daemon.alive for the pre-install liveness status.
    live: bool,
    copied: Mutex<Vec<(PathBuf, String)>>,
    calls: Mutex<Vec<RecordedCall>>,
}
/// (queue root, argv, stdin) observed by the fake transport.
type RecordedCall = (String, Vec<String>, Option<String>);

impl Transport for Stub {
    fn exec(&self, c: &ClientEntry, argv: &[String], stdin: Option<&str>) -> anyhow::Result<String> {
        self.calls.lock().unwrap().push((c.qroot.clone(), argv.to_vec(), stdin.map(str::to_string)));
        let j = argv.join(" ");
        if j.contains("ldd --version") {
            return Ok(self.ldd.into());
        }
        if j.contains(".shunt-owner") {
            return Ok(format!("{}\n", self.claim));
        }
        if j.contains("shunt.new") && j.contains("--version") {
            return match self.smoke {
                Smoke::Clean => Ok("shunt 0.2.0 (deadbee 2026-01-01)\n".into()),
                Smoke::Warn => Ok("/q/bin/shunt.new: loader noise line\nshunt 0.2.0 (deadbee 2026-01-01)\n".into()),
                Smoke::Fail => anyhow::bail!("/q/bin/shunt.new: error while loading shared libraries"),
            };
        }
        if j.contains("--version") {
            return Ok("shunt 0.2.0 (deadbee 2026-01-01)\n".into());
        }
        if j.contains("status") {
            return Ok(format!(r#"{{"protocol":1,"daemon":{{"alive":{live}}},"workers":{{"value":1,"source":"default"}},"running":[],"pending":[],"done_recent":[],"failed_recent":[],"stop":false,"shutdown":{{"armed":false,"grace_s":0,"cmd":""}}}}"#, live = self.live));
        }
        if j.contains("restart") {
            return Ok(r#"{"protocol":1,"ok":true}"#.into());
        }
        if j.contains("probe") {
            return Ok(r#"{"protocol":1,"cores":4}"#.into());
        }
        Ok(String::new()) // mkdir/chmod/mv/rm/init/start
    }
    fn copy_to(&self, _c: &ClientEntry, l: &std::path::Path, r: &str) -> anyhow::Result<()> {
        self.copied.lock().unwrap().push((l.to_path_buf(), r.to_string()));
        Ok(())
    }
}

fn stub(ldd: &'static str, claim: &'static str, smoke: Smoke) -> Stub {
    Stub { ldd, claim, smoke, live: false, copied: Mutex::new(Vec::new()), calls: Mutex::new(Vec::new()) }
}

/// Execs carrying the exact argv element `arg` — "start" and
/// "restart" share a substring, so counting needs equality.
fn calls_exact(t: &Stub, arg: &str) -> usize {
    t.calls.lock().unwrap().iter().filter(|(_, argv, _)| argv.iter().any(|a| a == arg)).count()
}

/// Row fixture with a REAL keypair — the .shunt-owner claim reads
/// `{key}.pub` off disk.
fn fixture(name: &str) -> (std::path::PathBuf, ClientEntry) {
    let tmp = std::env::temp_dir().join(format!("shunt-test-boot-{name}-{}", std::process::id()));
    std::env::set_var("SHUNT_HOME", &tmp);
    let mut e = base_entry();
    e.name = name.into();
    crate::server::client_ops::ensure_key(&mut e).unwrap();
    registry::save_client(&e).unwrap();
    (tmp, e)
}

/// Password-only bootstrap skips key generation and owner claim because
/// there is no public-key identity to deploy or claim with.
#[test]
fn bootstrap_password_only_skips_key_generation_and_owner_claim() {
    let _g = HOME_LOCK.lock().unwrap();
    let home = std::env::temp_dir().join(format!("shunt-test-boot-password-only-{}", std::process::id()));
    std::env::set_var("SHUNT_HOME", &home);
    let mut entry = base_entry();
    entry.name = "password-only".into();
    entry.ssh.password = "pw".into();
    registry::save_client(&entry).unwrap();

    let t = stub("ldd (GNU libc) 99.0", "FOREIGN", Smoke::Clean);
    bootstrap(&t, "password-only", None).unwrap();

    assert!(registry::load_client("password-only").unwrap().ssh.key.is_empty());
    assert!(!t.calls.lock().unwrap().iter().any(|(_, argv, _)| argv.iter().any(|arg| arg.contains(".shunt-owner"))), "without a pubkey the owner claim must be skipped");
    assert!(!home.join("keys/password-only").exists(), "password-only bootstrap must not generate a keypair");
    std::env::remove_var("SHUNT_HOME");
    std::fs::remove_dir_all(&home).ok();
}

/// §4.1 owner claim: a qroot already carrying a foreign
/// .shunt-owner refuses before the first write — two installs
/// must never merge into one queue.
#[test]
fn bootstrap_refuses_foreign_owned_qroot() {
    let _g = HOME_LOCK.lock().unwrap();
    let (tmp, _e) = fixture("foreign");
    let t = stub("ldd (GNU libc) 99.0", "FOREIGN", Smoke::Clean);
    let err = bootstrap(&t, "foreign", None).unwrap_err();
    std::env::remove_var("SHUNT_HOME");
    std::fs::remove_dir_all(&tmp).ok();
    assert!(err.to_string().contains("claimed by a different shunt install"), "{err}");
    assert!(t.copied.lock().unwrap().is_empty(), "refused before any copy");
}

/// §4.1: the claim SCRIPT itself, run against real `sh` —
/// create-exclusive (`set -C`) atomicity and the OWNED/FOREIGN
/// verdicts are the whole contract.
#[test]
fn claim_script_claims_then_refuses_foreign() {
    let dir = std::env::temp_dir().join(format!("shunt-claim-{}", std::process::id()));
    let owner = dir.join("deep/q/.shunt-owner");
    let run = |pubkey: &str| -> String {
        let out = std::process::Command::new("sh").args(["-c", CLAIM_SCRIPT, owner.to_str().unwrap(), pubkey]).output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    assert_eq!(run("ssh-ed25519 AAAAA a"), "CLAIMED", "fresh qroot claims");
    assert_eq!(run("ssh-ed25519 AAAAA a"), "OWNED", "same owner re-bootstrap");
    assert_eq!(run("ssh-ed25519 BBBBB b"), "FOREIGN", "foreign install refused");
    assert_eq!(run("ssh-ed25519 AAAAA a"), "OWNED", "owner survives the refusal");
    std::fs::remove_dir_all(&dir).ok();
}

/// §4.1: the queue carrying OUR OWN pub (a re-bootstrap of the
/// same install — the binary upgrade path) proceeds normally.
#[test]
fn bootstrap_reowns_same_pub() {
    let _g = HOME_LOCK.lock().unwrap();
    let (tmp, _e) = fixture("reown");
    let t = stub("ldd (GNU libc) 99.0", "OWNED", Smoke::Clean);
    bootstrap(&t, "reown", None).unwrap();
    std::env::remove_var("SHUNT_HOME");
    assert_eq!(t.copied.lock().unwrap().len(), 1);
    std::fs::remove_dir_all(&tmp).ok();
}

/// §15 error tier: an explicit --bin-path that cannot run on the
/// box is refused — staging it was required to observe the
/// refusal, but `mv` must never run and the staged file is
/// removed. Explicit paths never get the rescue substitution.
#[test]
fn bootstrap_refuses_unrunnable_explicit_bin() {
    let _g = HOME_LOCK.lock().unwrap();
    let (tmp, _e) = fixture("stub-old");
    let t = stub("ldd (GNU libc) 2.0", "CLAIMED", Smoke::Fail);
    let err = bootstrap(&t, "stub-old", Some("/bin/sh")).unwrap_err();
    std::env::remove_var("SHUNT_HOME");
    std::fs::remove_dir_all(&tmp).ok();
    let msg = format!("{err:#}");
    assert!(msg.contains("cannot run"), "{msg}");
    assert!(msg.contains("musl"), "{msg}");
    assert_eq!(t.copied.lock().unwrap().len(), 1, "staging precedes classification");
    assert_eq!(calls_exact(&t, "mv"), 0, "a failed candidate never installs");
    assert_eq!(calls_exact(&t, "rm"), 1, "the staged file is cleaned up");
}

/// §15 warn tier, explicit path: the binary runs but prints
/// loader noise — the operator's choice ships with a loud
/// report; rescue never substitutes an explicit path.
#[test]
fn bootstrap_warn_explicit_ships() {
    let _g = HOME_LOCK.lock().unwrap();
    let (tmp, _e) = fixture("stub-warn");
    let t = stub("ldd (GNU libc) 2.0", "CLAIMED", Smoke::Warn);
    bootstrap(&t, "stub-warn", Some("/bin/sh")).unwrap();
    std::env::remove_var("SHUNT_HOME");
    assert_eq!(t.copied.lock().unwrap().len(), 1);
    assert_eq!(calls_exact(&t, "mv"), 1);
    std::fs::remove_dir_all(&tmp).ok();
}

/// §15 warn tier, auto source: rescue is attempted first (the
/// musl release would upgrade warn → ok), but the test binary's
/// own --version is unparseable so rescue fails before any
/// network — the fallback is shipping the warning binary.
#[test]
fn bootstrap_warn_auto_falls_back_without_release() {
    let _g = HOME_LOCK.lock().unwrap();
    let (tmp, _e) = fixture("stub-warnauto");
    let t = stub("ldd (GNU libc) 2.0", "CLAIMED", Smoke::Warn);
    bootstrap(&t, "stub-warnauto", None).unwrap();
    std::env::remove_var("SHUNT_HOME");
    assert_eq!(t.copied.lock().unwrap().len(), 1, "rescue died before staging a second binary");
    assert_eq!(calls_exact(&t, "mv"), 1);
    std::fs::remove_dir_all(&tmp).ok();
}

/// §15 error tier, auto source: the rescue is the only road to
/// ok; when it fails (unparseable local --version, pre-network)
/// bootstrap bails without installing.
#[test]
fn bootstrap_error_auto_bails() {
    let _g = HOME_LOCK.lock().unwrap();
    let (tmp, _e) = fixture("stub-errauto");
    let t = stub("ldd (GNU libc) 2.0", "CLAIMED", Smoke::Fail);
    let err = bootstrap(&t, "stub-errauto", None).unwrap_err();
    std::env::remove_var("SHUNT_HOME");
    let msg = format!("{err:#}");
    assert!(msg.contains("musl"), "{msg}");
    assert_eq!(calls_exact(&t, "mv"), 0);
    assert_eq!(calls_exact(&t, "rm"), 1);
    std::fs::remove_dir_all(&tmp).ok();
}

/// The manual redeploy pathway: a live daemon is handed off via
/// wire `restart` (§3.5a) rather than left running the old image.
#[test]
fn bootstrap_live_daemon_handed_off() {
    let _g = HOME_LOCK.lock().unwrap();
    let (tmp, _e) = fixture("live");
    let mut t = stub("ldd (GNU libc) 99.0", "OWNED", Smoke::Clean);
    t.live = true;
    bootstrap(&t, "live", None).unwrap();
    std::env::remove_var("SHUNT_HOME");
    assert_eq!(calls_exact(&t, "restart"), 1);
    assert_eq!(calls_exact(&t, "start"), 0);
    std::fs::remove_dir_all(&tmp).ok();
}

/// §15 happy path: clean smoke → copy + mv, and a dead daemon
/// takes `start` rather than restart.
#[test]
fn bootstrap_compatible_box_ships() {
    let _g = HOME_LOCK.lock().unwrap();
    let (tmp, _e) = fixture("stub-ok");
    let t = stub("ldd (GNU libc) 99.0", "CLAIMED", Smoke::Clean);
    bootstrap(&t, "stub-ok", None).unwrap();
    std::env::remove_var("SHUNT_HOME");
    let copied = t.copied.lock().unwrap();
    assert_eq!(copied.len(), 1);
    assert_eq!(copied[0].1, "/q/bin/shunt.new");
    assert_eq!(copied[0].0, std::env::current_exe().unwrap());
    assert_eq!(calls_exact(&t, "start"), 1);
    assert_eq!(calls_exact(&t, "restart"), 0);
    std::fs::remove_dir_all(&tmp).ok();
}

#[test]
fn bootstrap_all_queues_applies_preferences_before_start_and_saves_base() {
    let _g = HOME_LOCK.lock().unwrap();
    let (tmp, mut entry) = fixture("multi");
    entry.qroot.clear();
    for (name, root, workers) in [("alpha", "/data/alpha", Some(7)), ("beta", "/data/beta", None)] {
        entry.queues.insert(name.into(), registry::QueueEntry { qroot: root.into(), env_setup: format!("export TARGET={name}"), shell: "bash".into(), exit_on_idle: true, workers });
    }
    registry::save_client(&entry).unwrap();
    let t = stub("ldd (GNU libc) 99.0", "OWNED", Smoke::Clean);
    bootstrap(&t, "multi", None).unwrap();
    let saved = registry::load_client("multi").unwrap();
    assert!(saved.qroot.is_empty() && saved.env_setup.is_empty() && saved.shell.is_empty());
    assert!(!saved.exit_on_idle);
    assert_eq!(saved.ssh.key, entry.ssh.key);
    assert_eq!(serde_json::to_value(&saved.queues).unwrap(), serde_json::to_value(&entry.queues).unwrap());
    assert_eq!(saved.default_queue, None);
    assert_eq!(saved.facts["cores"], 4);
    let calls = t.calls.lock().unwrap();
    assert_eq!(calls.iter().filter(|(_, argv, _)| argv.iter().any(|arg| arg == "probe")).count(), 1);
    for root in ["/data/alpha", "/data/beta"] {
        let scoped: Vec<_> = calls.iter().filter(|(qroot, _, _)| qroot == root).collect();
        let start = scoped.iter().position(|(_, argv, _)| argv.iter().any(|arg| arg == "start")).unwrap();
        let residency = scoped.iter().position(|(_, argv, content)| argv.last() == Some(&format!("{root}/exit_on_idle")) && content.as_deref() == Some("1\n")).unwrap();
        assert!(residency < start, "residency must precede the first task");
        let worker = scoped.iter().position(|(_, argv, _)| argv.iter().any(|arg| arg == "workers"));
        if root.ends_with("alpha") {
            assert!(worker.unwrap() < start);
            assert!(scoped[worker.unwrap()].1.windows(3).any(|args| args == ["workers", "set", "7"]));
        } else {
            assert!(worker.is_none(), "unset workers must preserve live tuning");
        }
    }
    assert_eq!(t.copied.lock().unwrap().iter().map(|(_, path)| path.as_str()).collect::<Vec<_>>(), ["/data/alpha/bin/shunt.new", "/data/beta/bin/shunt.new"]);
    std::env::remove_var("SHUNT_HOME");
    std::fs::remove_dir_all(tmp).ok();
}

#[test]
fn bootstrap_queue_ignores_execution_default_and_preserves_legacy_empty_env() {
    let _g = HOME_LOCK.lock().unwrap();
    let (tmp, mut entry) = fixture("selected");
    registry::canonicalize_queues(&mut entry).unwrap();
    entry.queues.insert("other".into(), registry::QueueEntry { qroot: "/data/other".into(), env_setup: String::new(), shell: String::new(), exit_on_idle: false, workers: None });
    registry::save_client(&entry).unwrap();
    let t = stub("ldd (GNU libc) 99.0", "OWNED", Smoke::Clean);
    bootstrap_queue(&t, "selected", "other", None).unwrap();
    let calls = t.calls.lock().unwrap();
    assert!(calls.iter().all(|(root, _, _)| root == "/data/other"));
    assert!(!calls.iter().any(|(_, argv, _)| argv.iter().any(|arg| arg.ends_with("/env_setup") || arg.ends_with("/shell") || arg.ends_with("/exit_on_idle"))));
    assert_eq!(registry::load_client("selected").unwrap().default_queue.as_deref(), Some("default"));
    std::env::remove_var("SHUNT_HOME");
    std::fs::remove_dir_all(tmp).ok();
}
