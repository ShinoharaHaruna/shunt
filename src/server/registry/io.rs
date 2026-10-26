//! Row load/save/list/remove, file headers, and the `localhost`
//! materialization rule.

use super::paths::shunt_home;
use super::types::*;
use crate::server::fsutil::{atomic_write, private_dir};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

/// Row types whose `name` must match their filename — see `load`.
trait RowName {
    fn row_name(&self) -> &str;
}

impl RowName for ClientEntry {
    fn row_name(&self) -> &str {
        &self.name
    }
}

impl RowName for ProjectEntry {
    fn row_name(&self) -> &str {
        &self.name
    }
}

fn load<T: for<'de> Deserialize<'de> + RowName>(dir: &str, name: &str) -> Result<T> {
    super::paths::valid_name(name)?;
    let path = shunt_home()?.join(dir).join(format!("{name}.toml"));
    let text = fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
    let row: T = toml::from_str(&text).with_context(|| format!("bad TOML in {}", path.display()))?;
    // The row's inner `name` — not the filename — is what joins into
    // cm/<name>.sock, state/<name>.fail, known_hosts/<name>, tunnels/
    // paths, and selects the localhost loopback transport. A forged
    // or mistyped name escapes $SHUNT_HOME ("../../x") or silently
    // re-routes the row ("localhost"). Registry files are
    // hand-editable, so a mismatch is surfaced, not auto-fixed.
    super::paths::valid_name(row.row_name()).with_context(|| format!("{}: inner name is not a valid registry name", path.display()))?;
    anyhow::ensure!(row.row_name() == name, "{}: inner name {:?} does not match filename {:?}", path.display(), row.row_name(), name);
    Ok(row)
}

/// Header written at the top of every client row file: documents the
/// knobs TOML omits when unset (Option fields serialize to nothing),
/// so a fresh row still shows the full template.
const CLIENT_HEADER: &str = "\
# shunt client row.
# Optional knobs, absent until set:
#   local_bin                          loopback transport: binary to exec (default: running shunt)
#   timeout_s                          loopback transport: exec timeout seconds
#   ssh_timeout_s                      ssh transport: ConnectTimeout (default 15)
#   ssh.password                       plaintext; key deploy or sshpass transport auth when ssh.key is empty
#   ssh.options                        per-client OpenSSH -o options (ProxyCommand/ProxyJump, etc.);
#                                      prefer `shunt client update <name> --option` — it kills the mux
#                                      master. A hand-edit rides the stale master until `ssh -O exit`
#                                      on cm/<name>.sock (under $SHUNT_HOME) or ControlPersist drains.
#   shutdown_cmd                       poweroff command pushed to <qroot>/shutdown_cmd at arm time
#   expire                             \"unlimited\" | \"metered\" | ISO-8601 timestamp (subscription end)
#   default_queue                      named queue used when --queue is omitted
#   queues.<name>.qroot                independent absolute queue root (must not overlap siblings)
#   queues.<name>.env_setup            shell snippet sourced before tasks (conda activate, exports)
#   queues.<name>.shell                task/env_setup shell: bash | zsh | sh (default bash)
#   queues.<name>.exit_on_idle         exit the daemon when this queue drains
#   queues.<name>.workers              optional worker preference (runtime file otherwise wins)
#   endpoint_rotates                   true if the vendor reassigns IP/port across restarts
#   billing.hourly / billing.currency  cost metadata surfaced to the agent
# Facts (gpu, cores, can_root, shutdown_cmd guess, ...) fill in via `shunt client probe`.
";

/// Header comment written above every project row — the files are
/// meant to be hand-editable, so fields self-document.
const PROJECT_HEADER: &str = "\
# shunt project row — edit by hand; `project add`/`bind` write this file.
#   name       registry key
#   desc       what this project is (helps agents pick the right one)
#   repo       canonical checkout on the dev machine (path or git URL)
#   [defaults].env  merged UNDER every binding's env at submit
#   [[bind]]   client + queue + remote workdir (repo location on the box) + env
#\n";

fn save_with_header<T: Serialize>(dir: &str, name: &str, value: &T, header: &str) -> Result<PathBuf> {
    super::paths::valid_name(name)?;
    let path = shunt_home()?.join(dir).join(format!("{name}.toml"));
    if let Some(parent) = path.parent() {
        private_dir(parent)?;
    }
    let text = format!("{header}{}", toml::to_string_pretty(value)?);
    // atomic_write: a concurrent `dispatch --once` sweep reading
    // the row mid-update must never see a truncated TOML.
    atomic_write(&path, text)?;
    Ok(path)
}

/// Load an existing row with identity validation, including disabled rows.
/// Lifecycle removal must not require a usable client or materialize defaults.
pub(crate) fn load_client_row(name: &str) -> Result<ClientEntry> {
    load("clients", name)
}

pub fn load_client(name: &str) -> Result<ClientEntry> {
    // `localhost` is a peer client (design.md §4.1): the first access
    // materializes clients/localhost.toml with the defaults
    // (qroot=$SHUNT_HOME/queue, deploy_root=$HOME), so it sits on disk
    // editable like any other row. The write is best-effort — an
    // unwritable HOME still yields the synthesized defaults. `local`
    // always stands for this name regardless of file contents.
    if name == "localhost" {
        let path = shunt_home()?.join("clients").join("localhost.toml");
        if !path.exists() {
            let entry = localhost_default()?;
            let _ = save_client(&entry); // materialize; best-effort
            return Ok(entry);
        }
        let mut entry = load_client_row(name)?;
        entry.local = true;
        return usable(entry);
    }
    usable(load_client_row(name)?)
}

/// Disabled rows are visible on disk but unusable as clients — and
/// so is an empty qroot: `shunt_bin` would render as `/bin/shunt`
/// (stage-then-mv into SYSTEM /bin), every wire op would pass
/// `--root ""`, and a loopback row would land queue dirs in the
/// local cwd (design.md §4.7 — the registry is authoritative for
/// where the queue lives; "" is not a location). Reject here so the
/// bad row degrades to an error row in `status --all` instead of
/// silently misrouting ops; `bootstrap` keeps its own earlier guard
/// for the nicer message context.
fn usable(entry: ClientEntry) -> Result<ClientEntry> {
    if entry.disabled {
        anyhow::bail!("client {} is disabled", entry.name);
    }
    super::queues::validate_queues(&entry)?;
    Ok(entry)
}

fn localhost_default() -> Result<ClientEntry> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(ClientEntry { name: "localhost".into(), local: true, disabled: false, local_bin: None, bin_path: None, timeout_s: None, ssh_timeout_s: None, pre_connect: String::new(), shutdown_cmd: None, expire: "unlimited".into(), env_setup: String::new(), shell: String::new(), endpoint_rotates: false, shared: false, exit_on_idle: false, ssh: SshEndpoint::default(), qroot: shunt_home()?.join("queue").display().to_string(), queues: BTreeMap::new(), default_queue: None, deploy_root: home, facts: BTreeMap::new(), billing: None })
}

/// Client names including the pre-registered `localhost`. Disabled
/// rows are skipped (they load, but are not usable clients).
pub fn list_clients() -> Result<Vec<String>> {
    let mut out = Vec::new();
    for n in list_names("clients")? {
        // load() not load_client(): disabled rows error out of
        // load_client, which would land in the Err arm and list anyway.
        let row: Result<ClientEntry> = load("clients", &n);
        match row {
            Ok(c) if !c.disabled => out.push(n),
            Ok(_) => {}            // disabled: stays off the list
            Err(_) => out.push(n), // unparseable row still lists
        }
    }
    // localhost absent on disk still lists (it materializes on use).
    if !out.iter().any(|n| n == "localhost") && !shunt_home()?.join("clients/localhost.toml").exists() {
        out.push("localhost".into());
        out.sort();
    }
    Ok(out)
}

pub fn save_client(entry: &ClientEntry) -> Result<PathBuf> {
    // Keep malformed rows writable for lifecycle repair/removal, but never
    // persist an execution overlay that would corrupt canonical configuration.
    super::queues::reject_mixed(entry)?;
    save_with_header("clients", &entry.name, entry, CLIENT_HEADER)
}

pub fn load_project(name: &str) -> Result<ProjectEntry> {
    load("projects", name)
}

pub fn save_project(entry: &ProjectEntry) -> Result<PathBuf> {
    save_with_header("projects", &entry.name, entry, PROJECT_HEADER)
}

pub fn load_server_config() -> Result<ServerConfig> {
    let path = shunt_home()?.join("server.toml");
    match fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("bad TOML in {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ServerConfig::default()),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

/// Surgical rewrite of `default_client` in `server.toml` for
/// `client rename` (`new = Some`) and `client rm` (`new = None`):
/// the name is load-bearing there too (design.md §4.1 — rename
/// "moves the whole identity"), and a dangling default fails every
/// `-c`-less command or silently retargets a future
/// re-registration of the old name. Line-level edit, not a
/// reserialize: server.toml is hand-editable and toml::to_string
/// would strip comments. Returns Ok(false) when nothing applies —
/// no file, a different/missing default, or a parsed key with no
/// matching bare line (e.g. `"default_client" = "x"` form, left
/// for the human). Bad TOML and IO errors propagate; callers treat
/// them as warn-only.
pub fn retarget_default_client(old: &str, new: Option<&str>) -> Result<bool> {
    let path = shunt_home()?.join("server.toml");
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    };
    // Parse first: only a valid config whose default really is `old`
    // is rewritten. A successful parse also guarantees at most one
    // top-level `default_client` (TOML rejects duplicate keys), so
    // the first bare-key line before any [section] is THE key.
    let cfg: ServerConfig = toml::from_str(&text).with_context(|| format!("bad TOML in {}", path.display()))?;
    if cfg.default_client.as_deref() != Some(old) {
        return Ok(false);
    }
    let mut out = String::with_capacity(text.len() + 16);
    let mut in_section = false;
    let mut done = false;
    for line in text.split_inclusive('\n') {
        let body = line.strip_suffix('\n').unwrap_or(line);
        let t = body.trim_start();
        if t.starts_with('[') {
            in_section = true;
        }
        let is_key = !done && !in_section && !t.starts_with('#') && t.strip_prefix("default_client").is_some_and(|rest| rest.trim_start().starts_with('='));
        if is_key {
            done = true;
            if let Some(n) = new {
                // Registry names are [A-Za-z0-9_.-] (valid_name) —
                // safe inside TOML quotes with no escaping. The
                // original indentation is kept; a trailing comment
                // on the line is dropped (rare, acceptable).
                out.push_str(&body[..body.len() - t.len()]);
                out.push_str(&format!("default_client = \"{n}\""));
                if line.ends_with('\n') {
                    out.push('\n');
                }
            } // else: rm — drop the line so required_client gives
              // the clean "no default" error.
            continue;
        }
        out.push_str(line);
    }
    if !done {
        return Ok(false);
    }
    // Same atomic-install discipline as save_with_header: a
    // concurrent reader must never see a truncated server.toml.
    atomic_write(&path, out)?;
    Ok(true)
}

/// List entry names under a registry subdir (`clients`, `projects`).
pub fn list_names(dir: &str) -> Result<Vec<String>> {
    let path = shunt_home()?.join(dir);
    let mut names = Vec::new();
    match fs::read_dir(&path) {
        Ok(rd) => {
            for ent in rd {
                let ent = ent?;
                let name = ent.file_name().to_string_lossy().into_owned();
                if let Some(stem) = name.strip_suffix(".toml") {
                    names.push(stem.to_string());
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
    names.sort();
    Ok(names)
}

pub fn remove(dir: &str, name: &str) -> Result<()> {
    super::paths::valid_name(name)?;
    let path = shunt_home()?.join(dir).join(format!("{name}.toml"));
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("no {dir} entry named {name}")
        }
        Err(e) => Err(e).with_context(|| format!("cannot remove {}", path.display())),
    }
}
