//! `client bootstrap` (design.md §7): key + binary ship + remote
//! init/start + facts merge. Doubles as the binary upgrade path.
//!
//! - `claim` — §4.1 `.shunt-owner` create-exclusive ownership claim
//! - `ship`  — per-queue stage/smoke/install + daemon handoff

mod claim;
mod ship;
#[cfg(test)]
mod tests;

use super::keys::{deploy_key, ensure_key};
use super::probe::probe_facts;
use crate::server::registry;
use crate::server::transport::Transport;
use anyhow::Result;

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
        match ship::bootstrap_resolved(t, &resolved, bin_path) {
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
