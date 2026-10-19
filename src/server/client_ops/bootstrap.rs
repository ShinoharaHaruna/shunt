//! `client bootstrap` (design.md §7): key + binary ship + remote
//! init/start + facts merge. Doubles as the binary upgrade path.

use super::env::push_env;
use super::keys::{deploy_key, ensure_key};
use super::probe::probe_facts;
use crate::server::registry;
use crate::server::transport::{shunt_bin, Transport};
use anyhow::{Context, Result};

/// `shunt client bootstrap <name>`: ensure key + try deploy, probe,
/// ship the local binary to `<qroot>/bin/shunt`, remote `init` +
/// `start`, push env_setup, merge facts.
pub fn bootstrap(t: &dyn Transport, name: &str) -> Result<()> {
    let mut entry = registry::load_client(name)?;
    // Any row: an empty qroot renders shunt_bin as `/bin/shunt` (the
    // stage-then-mv would target SYSTEM /bin) and wire ops pass
    // `--root ""`, landing queue dirs in the cwd.
    if entry.qroot.is_empty() {
        anyhow::bail!("{}: qroot is empty — set it in the row (usually <deploy_root>/queue)", entry.name);
    }
    ensure_key(&mut entry)?;
    if deploy_key(&entry)? {
        eprintln!("key deployed: {}", entry.ssh.key);
    } else if !entry.local && entry.ssh.password.is_empty() {
        eprintln!("note: no ssh.password in the row — install {}.pub on the box once\n      (vendor console or: ssh-copy-id -i {}.pub -p {} {}@{})", entry.ssh.key, entry.ssh.key, entry.ssh.port, entry.ssh.user, entry.ssh.host);
    }
    let bin = shunt_bin(&entry);
    let bindir = bin.rsplit_once('/').map(|(d, _)| d).unwrap_or("").to_string();
    t.exec(&entry, &["mkdir".into(), "-p".into(), bindir.clone()], None).context("mkdir remote bin dir")?;
    let local = std::env::current_exe().context("current_exe")?;
    // Stage-then-mv: scp truncates the destination in place, which
    // fails ETXTBSY while the daemon runs the old binary. Bootstrap
    // doubles as the binary upgrade path (init/start are idempotent),
    // so it must be safe to re-run on a live box.
    let staged = format!("{bin}.new");
    t.copy_to(&entry, &local, &staged).context("copy shunt binary")?;
    t.exec(&entry, &["chmod".into(), "+x".into(), staged.clone()], None)?;
    t.exec(&entry, &["mv".into(), "-f".into(), staged, bin.clone()], None).context("install shunt binary")?;

    t.exec(&entry, &[bin.clone(), "init".into(), "--root".into(), entry.qroot.clone(), "--json".into()], None).context("remote shunt init")?;
    t.exec(&entry, &[bin.clone(), "start".into(), "--root".into(), entry.qroot.clone(), "--json".into()], None).context("remote shunt start")?;

    // Probe AFTER the binary lands: the wire `shunt probe` is the
    // single facts implementation (design.md §8).
    let facts = probe_facts(t, &entry)?;
    entry.facts.extend(facts);

    // env_setup/shell only push when the row actually carries them;
    // an empty push would clobber a box-side edit, which is not the
    // contract bootstrap has with the registry.
    if !entry.env_setup.is_empty() || !entry.shell.is_empty() {
        if let Err(e) = push_env(t, name) {
            eprintln!("note: env_setup push failed: {e:#}");
        }
    }
    let p = registry::save_client(&entry)?;
    println!("bootstrapped {} ({})", entry.name, bin);
    println!("wrote {}", p.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::bootstrap;
    use crate::server::registry::{self, tests::HOME_LOCK, ClientEntry};
    use crate::server::transport::Transport;

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
        let err = bootstrap(&Panic, "loctest").unwrap_err();
        std::env::remove_var("SHUNT_HOME");
        std::fs::remove_dir_all(&tmp).ok();
        assert!(err.to_string().contains("qroot is empty"), "{err}");
    }
}
