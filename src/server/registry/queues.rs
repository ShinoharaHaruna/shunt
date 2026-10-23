//! Named queue validation and explicit configuration mutations.

use super::names::{self, Resolution};
use super::{load_client, save_client, valid_name, ClientEntry, QueueEntry};
use anyhow::{bail, ensure, Context, Result};
use std::path::{Component, Path, PathBuf};

pub const DEFAULT_QUEUE: &str = "default";

pub fn default_queue_name() -> String {
    DEFAULT_QUEUE.into()
}

/// Ephemeral execution configuration. Persist the separately loaded base row,
/// never this client: its legacy fields deliberately overlay the queue map.
#[derive(Debug, Clone)]
pub struct ResolvedQueue {
    pub name: String,
    pub client: ClientEntry,
    pub workers: Option<u32>,
}

pub fn queue_names(client: &ClientEntry) -> Vec<String> {
    if client.queues.is_empty() {
        vec![default_queue_name()]
    } else {
        client.queues.keys().cloned().collect()
    }
}

pub(super) fn reject_mixed(client: &ClientEntry) -> Result<()> {
    ensure!(client.queues.is_empty() || (client.qroot.is_empty() && client.env_setup.is_empty() && client.shell.is_empty() && !client.exit_on_idle), "{}: mixed canonical queues and legacy queue fields; clear top-level qroot/env_setup/shell/exit_on_idle and save the base client row, not a resolved execution view", client.name);
    Ok(())
}

pub(super) fn validate_queues(client: &ClientEntry) -> Result<()> {
    reject_mixed(client)?;
    if client.queues.is_empty() {
        ensure!(!client.qroot.trim().is_empty(), "{}: qroot is empty — set it in the row (usually <deploy_root>/queue)", client.name);
    } else {
        let mut roots: Vec<(&str, PathBuf)> = Vec::new();
        for (name, queue) in &client.queues {
            valid_name(name).with_context(|| format!("{}: invalid queue name; rename the [queues.<name>] table", client.name))?;
            let root = validate_qroot(&client.name, name, &queue.qroot)?;
            for (other_name, other_root) in &roots {
                ensure!(!root.starts_with(other_root) && !other_root.starts_with(&root), "{}: queue {name:?} qroot {:?} overlaps queue {other_name:?} qroot {:?}; configure separate, non-nested absolute qroots", client.name, queue.qroot, other_root);
            }
            roots.push((name, root));
        }
    }
    if let Some(name) = &client.default_queue {
        valid_name(name).with_context(|| format!("{}: invalid default_queue; set it to a registered queue name", client.name))?;
        ensure!(has_queue(client, name), "{}: default_queue {name:?} is not registered; set default_queue to one of: {}", client.name, queue_names(client).join(", "));
    }
    Ok(())
}

fn validate_qroot(client: &str, name: &str, qroot: &str) -> Result<PathBuf> {
    ensure!(!qroot.is_empty(), "{client}: queue {name:?} qroot is empty; set an absolute qroot");
    ensure!(qroot.trim() == qroot, "{client}: queue {name:?} qroot has surrounding whitespace; remove it");
    let path = Path::new(qroot);
    ensure!(path.is_absolute(), "{client}: queue {name:?} qroot must be absolute; set an absolute qroot");
    ensure!(!path.components().any(|part| part == Component::ParentDir), "{client}: queue {name:?} qroot contains parent traversal; set an absolute path without '..'");
    // Components normalize repeated separators and '.', without consulting a
    // local filesystem that is unrelated to the remote queue's filesystem.
    Ok(path.components().collect())
}

fn has_queue(client: &ClientEntry, name: &str) -> bool {
    if client.queues.is_empty() {
        name == DEFAULT_QUEUE
    } else {
        client.queues.contains_key(name)
    }
}

fn execution_view(client: &ClientEntry, name: &str) -> ResolvedQueue {
    let mut execution = client.clone();
    let workers = if let Some(queue) = client.queues.get(name) {
        execution.qroot.clone_from(&queue.qroot);
        execution.env_setup.clone_from(&queue.env_setup);
        execution.shell.clone_from(&queue.shell);
        execution.exit_on_idle = queue.exit_on_idle;
        queue.workers
    } else {
        None
    };
    ResolvedQueue { name: name.into(), client: execution, workers }
}

/// `--queue` > configured `default_queue` > sole queue — the picked
/// name then resolves through the registry name middleware (§4.1):
/// a case-folded hit substitutes the canonical spelling with a
/// stderr note, and a miss names the registered queues plus any
/// did-you-mean suggestions.
pub fn resolve_queue(client: &ClientEntry, queue: Option<&str>) -> Result<ResolvedQueue> {
    validate_queues(client)?;
    let registered = queue_names(client);
    let name = match queue.or(client.default_queue.as_deref()) {
        Some(name) => name,
        None if registered.len() == 1 => &registered[0],
        None => bail!("{}: queue target is ambiguous ({}); pass --queue NAME or run `shunt queue default NAME -c {}`", client.name, registered.join(", "), client.name),
    };
    valid_name(name).context("invalid queue target; pass a registered --queue NAME")?;
    match names::resolve(name, &registered) {
        Resolution::Exact(n) => Ok(execution_view(client, &n)),
        Resolution::Canonical { canonical, typed } => {
            eprintln!("note: resolved '{typed}' → queue '{canonical}' on client '{}'", client.name);
            Ok(execution_view(client, &canonical))
        }
        Resolution::Unknown { input, suggestions } => bail!("{}: unknown queue {input:?}; choose --queue from: {} (list with `shunt queue ls -c {}`){}", client.name, registered.join(", "), client.name, names::did_you_mean(&suggestions)),
        Resolution::Ambiguous(cands) => bail!("{}: queue {name:?} is ambiguous — registered names differ only by case: {}", client.name, cands.join(", ")),
    }
}

pub fn load_queue(client_name: &str, queue: Option<&str>) -> Result<ResolvedQueue> {
    resolve_queue(&load_client(client_name)?, queue)
}

pub fn all_queues(client: &ClientEntry) -> Result<Vec<ResolvedQueue>> {
    validate_queues(client)?;
    Ok(queue_names(client).iter().map(|name| execution_view(client, name)).collect())
}

pub fn canonicalize_queues(client: &mut ClientEntry) -> Result<()> {
    validate_queues(client)?;
    if !client.queues.is_empty() {
        return Ok(());
    }
    let mut canonical = client.clone();
    let queue = QueueEntry { qroot: std::mem::take(&mut canonical.qroot), env_setup: std::mem::take(&mut canonical.env_setup), shell: std::mem::take(&mut canonical.shell), exit_on_idle: std::mem::take(&mut canonical.exit_on_idle), workers: None };
    canonical.queues.insert(default_queue_name(), queue);
    canonical.default_queue = Some(default_queue_name());
    validate_queues(&canonical)?;
    *client = canonical;
    Ok(())
}

pub fn add_queue(client_name: &str, name: &str, entry: QueueEntry) -> Result<PathBuf> {
    valid_name(name)?;
    let mut client = load_client(client_name)?;
    canonicalize_queues(&mut client)?;
    ensure!(!client.queues.contains_key(name), "{client_name}: queue {name:?} already exists; choose another queue name");
    client.queues.insert(name.into(), entry);
    validate_queues(&client)?;
    save_client(&client)
}

pub fn set_default_queue(client_name: &str, name: &str) -> Result<PathBuf> {
    let mut client = load_client(client_name)?;
    resolve_queue(&client, Some(name))?;
    canonicalize_queues(&mut client)?;
    client.default_queue = Some(name.into());
    save_client(&client)
}

/// Call only after the lifecycle owner has checked bindings and runtime work.
pub fn remove_queue_config(client_name: &str, name: &str) -> Result<PathBuf> {
    let mut client = load_client(client_name)?;
    resolve_queue(&client, Some(name))?;
    canonicalize_queues(&mut client)?;
    ensure!(client.queues.len() > 1, "{client_name}: cannot remove the last queue; remove the client instead");
    ensure!(client.default_queue.as_deref() != Some(name), "{client_name}: cannot remove configured default queue {name:?}; run `shunt queue default OTHER -c {client_name}` first");
    client.queues.remove(name);
    validate_queues(&client)?;
    save_client(&client)
}
