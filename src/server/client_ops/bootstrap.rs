//! `client bootstrap` (design.md §7): key + binary ship + remote
//! init/start + facts merge. Doubles as the binary upgrade path.

use super::env::push_resolved_env;
use super::keys::{deploy_key, ensure_key};
use super::probe::probe_facts;
use super::{compat, release};
use crate::server::registry::{self, ClientEntry, ResolvedQueue};
use crate::server::transport::{shell_quote, shunt_bin, Transport};
use anyhow::{Context, Result};
use std::path::PathBuf;

/// The musl build recipe named in every incompatibility error.
const MUSL_RECIPE: &str = "rustup target add x86_64-unknown-linux-musl && cargo build --release --target x86_64-unknown-linux-musl";

/// Bootstrap every registered queue, sharing one instance identity and facts probe.
pub fn bootstrap(t: &dyn Transport, name: &str, bin_path: Option<&str>) -> Result<()> {
    bootstrap_selected(t, name, None, bin_path)?;
    println!("bootstrapped {name}");
    Ok(())
}

/// Bootstrap one queue without changing the instance's configured default.
pub fn bootstrap_queue(t: &dyn Transport, name: &str, queue: &str, bin_path: Option<&str>) -> Result<()> {
    bootstrap_selected(t, name, Some(queue), bin_path)
}

fn bootstrap_selected(t: &dyn Transport, name: &str, queue: Option<&str>, bin_path: Option<&str>) -> Result<()> {
    let mut entry = registry::load_client(name)?;
    // Validate every target before generating keys or contacting the instance.
    let targets = match queue {
        Some(q) => vec![registry::resolve_queue(&entry, Some(q))?],
        None => registry::all_queues(&entry)?,
    };
    let original_key = entry.ssh.key.clone();
    ensure_key(&mut entry)?;
    if entry.ssh.key != original_key {
        registry::save_client(&entry)?;
    }
    if deploy_key(&entry)? {
        eprintln!("key deployed: {}", entry.ssh.key);
    } else if !entry.local && entry.ssh.password.is_empty() || entry.shared {
        let why = if entry.shared { "shared row — authorized_keys is never modified automatically" } else { "no ssh.password in the row" };
        eprintln!("note: {why} — install {}.pub on the box once\n      (vendor console or: ssh-copy-id -i {}.pub -p {} {}@{})", entry.ssh.key, entry.ssh.key, entry.ssh.port, entry.ssh.user, entry.ssh.host);
    }

    let mut failures = Vec::new();
    let mut probe_target = None;
    for target in targets {
        // Resolve again after key generation; execution views must never be saved.
        let resolved = registry::resolve_queue(&entry, Some(&target.name))?;
        match bootstrap_resolved(t, &resolved, bin_path) {
            Ok(()) => {
                if probe_target.is_none() {
                    probe_target = Some(resolved);
                }
            }
            Err(err) => failures.push(format!("{}: {err:#}", target.name)),
        }
    }
    if let Some(target) = probe_target {
        match probe_facts(t, &target.client) {
            Ok(facts) => entry.facts.extend(facts),
            Err(err) => failures.push(format!("probe: {err:#}")),
        }
    }
    let path = registry::save_client(&entry)?;
    eprintln!("wrote {}", path.display());
    anyhow::ensure!(failures.is_empty(), "bootstrap incomplete for {name}: {}", failures.join("; "));
    Ok(())
}

fn bootstrap_resolved(t: &dyn Transport, queue: &ResolvedQueue, bin_path: Option<&str>) -> Result<()> {
    let entry = &queue.client;
    let name = &entry.name;
    // .shunt-owner claim (§4.1): two independent shunt installs
    // pointed at the same qroot silently merge into one queue —
    // seq races between two dispatchers, one daemon serving both.
    // One atomic remote script, BEFORE any write: `set -C` gives
    // create-exclusive so a simultaneous claim cannot interleave —
    // a later bootstrap can never preempt an earlier owner's qroot.
    if let Some(pubkey) = owner_pubkey(entry) {
        let owner = owner_path(entry);
        let verdict = t.exec(entry, &["sh".into(), "-c".into(), CLAIM_SCRIPT.into(), owner, pubkey], None).context("claim .shunt-owner")?;
        let verdict = verdict.trim();
        if verdict == "FOREIGN" {
            anyhow::bail!("qroot {} is claimed by a different shunt install (.shunt-owner mismatch) — pick a different qroot in the row; if the other install is gone, remove {}", entry.qroot, owner_path(entry));
        }
        anyhow::ensure!(matches!(verdict, "OWNED" | "CLAIMED"), "unexpected .shunt-owner claim reply: {verdict:?}");
    }
    let bin = shunt_bin(entry);
    let bindir = bin.rsplit_once('/').map(|(d, _)| d).unwrap_or("").to_string();
    t.exec(entry, &["mkdir".into(), "-p".into(), bindir.clone()], None).context("mkdir remote bin dir")?;

    // §15: pick the binary, verify the box's loader can serve it,
    // rescue via the matching musl release when it can't.
    let explicit = bin_path.map(PathBuf::from).or_else(|| entry.bin_path.as_deref().map(PathBuf::from));
    let local = match &explicit {
        Some(p) => {
            anyhow::ensure!(p.is_file(), "--bin-path/bin_path {}: no such file", p.display());
            p.clone()
        }
        None => std::env::current_exe().context("current_exe")?,
    };
    let needs = compat::required_glibc(&local)?;
    let mut ship = local.clone();
    let mut _rescue = None; // keeps the downloaded TempBin alive until copy_to ran
    if needs.is_some() {
        if let compat::Verdict::Incompatible(why) = compat::check(needs, compat::remote_libc(t, entry).as_ref()) {
            if explicit.is_some() {
                anyhow::bail!("{why} — {} cannot run on {name}; ship a libc-independent build: {MUSL_RECIPE}", local.display());
            }
            match release::rescue_musl(&local) {
                Ok(tb) => {
                    eprintln!("note: {why} — shipping the matching musl release build");
                    ship = tb.path().to_path_buf();
                    _rescue = Some(tb);
                }
                Err(e2) => anyhow::bail!("{why}; release rescue failed ({e2:#}). Build one: {MUSL_RECIPE} → `client bootstrap {name} --bin-path target/x86_64-unknown-linux-musl/release/shunt`"),
            }
        }
    }
    if explicit.is_some() {
        // bin_path persists across re-bootstraps (the upgrade path) —
        // print what's being shipped vs what we run so drift is loud.
        let v = |p: &PathBuf| std::process::Command::new(p).arg("--version").output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_else(|| "?".into());
        eprintln!("ship {} ({})", ship.display(), v(&ship));
    }

    // Stage-then-mv: scp truncates the destination in place, which
    // fails ETXTBSY while the daemon runs the old binary. Bootstrap
    // doubles as the binary upgrade path (init/start are idempotent),
    // so it must be safe to re-run on a live box.
    let staged = format!("{bin}.new");
    t.copy_to(entry, &ship, &staged).context("copy shunt binary")?;
    t.exec(entry, &["chmod".into(), "+x".into(), staged.clone()], None)?;
    t.exec(entry, &["mv".into(), "-f".into(), staged, bin.clone()], None).context("install shunt binary")?;

    // Post-install smoke (§15): `exec` returns stdout only, so the
    // loader's stderr (weak-version warnings) is merged in via 2>&1.
    let smoke = t.exec(entry, &["sh".into(), "-c".into(), format!("{} --version 2>&1", shell_quote(&bin))], None).context("shipped binary cannot run on the box")?;
    for line in smoke.lines() {
        if line.trim_start().starts_with("shunt ") {
            eprintln!("remote: {}", line.trim());
        } else if !line.trim().is_empty() {
            eprintln!("warning (box): {line}");
        }
    }

    t.exec(entry, &[bin.clone(), "init".into(), "--root".into(), entry.qroot.clone(), "--json".into()], None).context("remote shunt init")?;
    // Apply residency and worker preferences before the daemon can claim its first task.
    // Empty legacy preferences preserve box-side edits during bootstrap.
    if !entry.env_setup.is_empty() || !entry.shell.is_empty() || entry.exit_on_idle || queue.workers.is_some() {
        push_resolved_env(t, queue)?;
    } else {
        crate::server::tunnel::sync_queue_env(t, &entry.name, &queue.name)?;
    }
    t.exec(entry, &[bin.clone(), "start".into(), "--root".into(), entry.qroot.clone(), "--json".into()], None).context("remote shunt start")?;
    eprintln!("bootstrapped {} queue {} ({})", entry.name, queue.name, bin);
    Ok(())
}

/// `<qroot>/.shunt-owner` claim as one remote script (§4.1).
/// `$0` is the owner-file path, `$1` our pubkey line. Answers a
/// marker word: OWNED (file already holds our key), CLAIMED (we
/// wrote it), FOREIGN (another install's key — refuse). `set -C`
/// (O_EXCL) makes the create atomic against a racing claimer.
const CLAIM_SCRIPT: &str = r#"o="$0"; if [ -f "$o" ]; then grep -qxF "$1" "$o" && echo OWNED || echo FOREIGN; exit 0; fi; mkdir -p "${o%/*}"; if ( set -C; printf '%s\n' "$1" > "$o" ) 2>/dev/null; then echo CLAIMED; else grep -qxF "$1" "$o" 2>/dev/null && echo OWNED || echo FOREIGN; fi"#;

/// `<qroot>/.shunt-owner` — holds the claiming client's ssh public
/// key line; bootstrap refuses a foreign claim (§4.1).
fn owner_path(e: &ClientEntry) -> String {
    format!("{}/.shunt-owner", e.qroot)
}

/// The pubkey identifying this install: `{ssh.key}.pub` when it
/// exists, else `ssh-keygen -y` derivation. `None` means no identity
/// to claim with (e.g. `--key` given without a derivable pub) — the
/// caller then skips the claim check.
fn owner_pubkey(e: &ClientEntry) -> Option<String> {
    if e.ssh.key.is_empty() {
        return None;
    }
    let pub_path = format!("{}.pub", e.ssh.key);
    if let Ok(s) = std::fs::read_to_string(&pub_path) {
        let s = s.trim();
        if !s.is_empty() {
            return Some(s.to_string());
        }
    }
    let out = std::process::Command::new("ssh-keygen").args(["-y", "-f", &e.ssh.key]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then(|| format!("{s} shunt-{}", e.name))
}

#[cfg(test)]
mod tests {
    use super::bootstrap;
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

    /// Canned answers per argv: `ldd --version` decides the compat
    /// verdict; `cat .../.shunt-owner` reports the fixture claim;
    /// copy_to is recorded so tests can assert nothing was shipped
    /// on refusal.
    struct Stub {
        ldd: &'static str,
        /// Marker the .shunt-owner claim script answers.
        claim: &'static str,
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
            if j.contains("--version") {
                return Ok("shunt 0.2.0 (deadbee 2026-01-01)\n".into());
            }
            if j.contains("probe") {
                return Ok(r#"{"protocol":1,"cores":4}"#.into());
            }
            Ok(String::new()) // mkdir/chmod/mv/init/start
        }
        fn copy_to(&self, _c: &ClientEntry, l: &std::path::Path, r: &str) -> anyhow::Result<()> {
            self.copied.lock().unwrap().push((l.to_path_buf(), r.to_string()));
            Ok(())
        }
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

    /// §4.1 owner claim: a qroot already carrying a foreign
    /// .shunt-owner refuses before the first write — two installs
    /// must never merge into one queue.
    #[test]
    fn bootstrap_refuses_foreign_owned_qroot() {
        let _g = HOME_LOCK.lock().unwrap();
        let (tmp, _e) = fixture("foreign");
        let t = Stub { ldd: "ldd (GNU libc) 99.0", claim: "FOREIGN", copied: Mutex::new(Vec::new()), calls: Mutex::new(Vec::new()) };
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
            let out = std::process::Command::new("sh").args(["-c", super::CLAIM_SCRIPT, owner.to_str().unwrap(), pubkey]).output().unwrap();
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
        let t = Stub { ldd: "ldd (GNU libc) 99.0", claim: "OWNED", copied: Mutex::new(Vec::new()), calls: Mutex::new(Vec::new()) };
        bootstrap(&t, "reown", None).unwrap();
        std::env::remove_var("SHUNT_HOME");
        assert_eq!(t.copied.lock().unwrap().len(), 1);
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// §15 refuse path: an explicit --bin-path needing a newer glibc
    /// than the box serves is rejected before anything is copied —
    /// explicit paths never get the release-rescue substitution.
    /// The fixture is a real glibc-linked binary (`/bin/sh`): a fake
    /// `GLIBC_x.y` literal embedded in this source would sit inside
    /// the test binary itself and poison required_glibc(current_exe)
    /// in the sibling test (observed — string pooling places adjacent
    /// byte literals contiguously).
    #[test]
    fn bootstrap_refuses_incompatible_explicit_bin() {
        let _g = HOME_LOCK.lock().unwrap();
        let (tmp, _e) = fixture("stub-old");
        let probe = PathBuf::from("/bin/sh");
        let needs = crate::server::client_ops::compat::required_glibc(&probe).unwrap();
        assert!(needs.is_some(), "/bin/sh must be glibc-linked for this test");
        let t = Stub { ldd: "ldd (GNU libc) 2.0", claim: "CLAIMED", copied: Mutex::new(Vec::new()), calls: Mutex::new(Vec::new()) };
        let err = bootstrap(&t, "stub-old", Some("/bin/sh")).unwrap_err();
        std::env::remove_var("SHUNT_HOME");
        std::fs::remove_dir_all(&tmp).ok();
        let msg = format!("{err:#}");
        assert!(msg.contains("GLIBC"), "{msg}");
        assert!(msg.contains("musl"), "{msg}");
        assert!(t.copied.lock().unwrap().is_empty(), "refused bootstrap must not copy");
    }

    /// §15 happy path: box glibc newer than required → copy happens,
    /// then the smoke `--version 2>&1` runs and echo prints.
    #[test]
    fn bootstrap_compatible_box_ships() {
        let _g = HOME_LOCK.lock().unwrap();
        let (tmp, _e) = fixture("stub-ok");
        let t = Stub { ldd: "ldd (GNU libc) 99.0", claim: "CLAIMED", copied: Mutex::new(Vec::new()), calls: Mutex::new(Vec::new()) };
        bootstrap(&t, "stub-ok", None).unwrap();
        std::env::remove_var("SHUNT_HOME");
        let copied = t.copied.lock().unwrap();
        assert_eq!(copied.len(), 1);
        assert_eq!(copied[0].1, "/q/bin/shunt.new");
        assert_eq!(copied[0].0, std::env::current_exe().unwrap());
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
        let t = Stub { ldd: "ldd (GNU libc) 99.0", claim: "OWNED", copied: Mutex::new(Vec::new()), calls: Mutex::new(Vec::new()) };
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
        let t = Stub { ldd: "ldd (GNU libc) 99.0", claim: "OWNED", copied: Mutex::new(Vec::new()), calls: Mutex::new(Vec::new()) };
        super::bootstrap_queue(&t, "selected", "other", None).unwrap();
        let calls = t.calls.lock().unwrap();
        assert!(calls.iter().all(|(root, _, _)| root == "/data/other"));
        assert!(!calls.iter().any(|(_, argv, _)| argv.iter().any(|arg| arg.ends_with("/env_setup") || arg.ends_with("/shell") || arg.ends_with("/exit_on_idle"))));
        assert_eq!(registry::load_client("selected").unwrap().default_queue.as_deref(), Some("default"));
        std::env::remove_var("SHUNT_HOME");
        std::fs::remove_dir_all(tmp).ok();
    }
}
