//! Registry: human-editable TOML rows under `~/.config/shunt/`
//! (`$SHUNT_HOME` overrides for tests). Load/save here is real; the
//! rest of the server is stubbed.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

/// One experiment box (design.md §4.1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientEntry {
    pub name: String,
    /// Local loopback client: transport execs the local `shunt` binary
    /// instead of SSH. Always true for the built-in `localhost` row.
    #[serde(default)]
    pub local: bool,
    /// Opt-out: a disabled row stays on disk but is skipped by
    /// `list_clients`/`status --all` and rejected by `load_client`.
    /// This is how a user drops the pre-registered `localhost` — `rm`
    /// only resets it to factory defaults (the row regenerates).
    #[serde(default)]
    pub disabled: bool,
    /// Local transport: binary to exec for wire ops (default: the
    /// running `shunt` binary).
    #[serde(default)]
    pub local_bin: Option<String>,
    /// Local transport: exec timeout in seconds.
    #[serde(default)]
    pub timeout_s: Option<u64>,
    /// SSH transport: connect timeout override in seconds (default 10).
    #[serde(default)]
    pub ssh_timeout_s: Option<u64>,
    /// Command this box execs when idle-shutdown fires. Pushed to
    /// `<qroot>/shutdown_cmd` by `shutdown -c <name> on`; the daemon's
    /// last-moment resolution uses it before falling back to
    /// `$SHUTDOWN_CMD` and `/usr/bin/shutdown` (design.md §3.11).
    /// Point it at `true`/a marker script in test environments.
    #[serde(default)]
    pub shutdown_cmd: Option<String>,
    #[serde(default)]
    pub ssh: SshEndpoint,
    /// Queue dir on the box.
    pub qroot: String,
    /// Vendor work root (AutoDL: `/root/autodl-tmp`).
    pub deploy_root: String,
    /// Facts filled by `probe`/`bootstrap`; advisory only.
    #[serde(default)]
    pub facts: BTreeMap<String, serde_json::Value>,
    /// Cost metadata surfaced to the agent (design.md §9).
    #[serde(default)]
    pub billing: Option<Billing>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SshEndpoint {
    #[serde(default)]
    pub host: String,
    /// 0 = unset (port may genuinely be 0 in a template before fill-in).
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Billing {
    pub hourly: f64,
    pub currency: String,
}

/// One project; many-to-many with clients via `[[bind]]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectEntry {
    pub name: String,
    #[serde(default)]
    pub bind: Vec<Binding>,
}

/// A binding supplies workdir + env overlay for tasks on that client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Binding {
    pub client: String,
    pub workdir: String,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// Global server defaults (poll interval, default client).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub poll_interval_s: Option<u64>,
    pub default_client: Option<String>,
}

/// Registry home: `$SHUNT_HOME` else `~/.config/shunt`.
pub fn shunt_home() -> Result<PathBuf> {
    if let Ok(h) = std::env::var("SHUNT_HOME") {
        return Ok(PathBuf::from(h));
    }
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".config/shunt"))
}

fn load<T: for<'de> Deserialize<'de>>(dir: &str, name: &str) -> Result<T> {
    let path = shunt_home()?.join(dir).join(format!("{name}.toml"));
    let text =
        fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("bad TOML in {}", path.display()))
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
#   billing.hourly / billing.currency  cost metadata surfaced to the agent
# Facts (gpu, cores, can_root, ...) fill in via `shunt client probe`.
";

fn save_with_header<T: Serialize>(
    dir: &str,
    name: &str,
    value: &T,
    header: &str,
) -> Result<PathBuf> {
    let path = shunt_home()?.join(dir).join(format!("{name}.toml"));
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = format!("{header}{}", toml::to_string_pretty(value)?);
    fs::write(&path, text)?;
    Ok(path)
}

fn save<T: Serialize>(dir: &str, name: &str, value: &T) -> Result<PathBuf> {
    save_with_header(dir, name, value, "")
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
        let mut entry: ClientEntry = load("clients", name)?;
        entry.local = true;
        return usable(entry);
    }
    usable(load("clients", name)?)
}

/// Disabled rows are visible on disk but unusable as clients.
fn usable(entry: ClientEntry) -> Result<ClientEntry> {
    if entry.disabled {
        anyhow::bail!("client {} is disabled", entry.name);
    }
    Ok(entry)
}

fn localhost_default() -> Result<ClientEntry> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(ClientEntry {
        name: "localhost".into(),
        local: true,
        disabled: false,
        local_bin: None,
        timeout_s: None,
        ssh_timeout_s: None,
        shutdown_cmd: None,
        ssh: SshEndpoint::default(),
        qroot: shunt_home()?.join("queue").display().to_string(),
        deploy_root: home,
        facts: BTreeMap::new(),
        billing: None,
    })
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
    if !out.iter().any(|n| n == "localhost")
        && !shunt_home()?.join("clients/localhost.toml").exists()
    {
        out.push("localhost".into());
        out.sort();
    }
    Ok(out)
}

pub fn save_client(entry: &ClientEntry) -> Result<PathBuf> {
    save_with_header("clients", &entry.name, entry, CLIENT_HEADER)
}

pub fn load_project(name: &str) -> Result<ProjectEntry> {
    load("projects", name)
}

pub fn save_project(entry: &ProjectEntry) -> Result<PathBuf> {
    save("projects", &entry.name, entry)
}

pub fn load_server_config() -> Result<ServerConfig> {
    let path = shunt_home()?.join("server.toml");
    match fs::read_to_string(&path) {
        Ok(text) => {
            toml::from_str(&text).with_context(|| format!("bad TOML in {}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ServerConfig::default()),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
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
    let path = shunt_home()?.join(dir).join(format!("{name}.toml"));
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("no {dir} entry named {name}")
        }
        Err(e) => Err(e).with_context(|| format!("cannot remove {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_roundtrip() {
        let tmp = std::env::temp_dir().join(format!("shunt-test-{}", std::process::id()));
        std::env::set_var("SHUNT_HOME", &tmp);
        let entry = ClientEntry {
            name: "box-1".into(),
            local: false,
            disabled: false,
            local_bin: None,
            timeout_s: None,
            ssh_timeout_s: None,
            shutdown_cmd: None,
            ssh: SshEndpoint {
                host: "example.invalid".into(),
                port: 22,
                user: "root".into(),
                key: "/path/to/key".into(),
            },
            qroot: "/root/autodl-tmp/queue".into(),
            deploy_root: "/root/autodl-tmp".into(),
            facts: BTreeMap::new(),
            billing: Some(Billing {
                hourly: 2.6,
                currency: "cny".into(),
            }),
        };
        save_client(&entry).unwrap();
        let back = load_client("box-1").unwrap();
        assert_eq!(back.ssh.port, 22);
        assert_eq!(list_names("clients").unwrap(), vec!["box-1"]);

        // localhost is a peer: first access materializes its row file
        // with the defaults, and it lists like any other client.
        let lo = load_client("localhost").unwrap();
        assert!(lo.local);
        assert_eq!(lo.qroot, tmp.join("queue").display().to_string());
        assert!(tmp.join("clients/localhost.toml").exists());
        let lo2 = load_client("localhost").unwrap(); // second load: from disk
        assert_eq!(lo2.qroot, lo.qroot);
        assert_eq!(list_clients().unwrap(), vec!["box-1", "localhost"]);

        // disabled=true opts out of being a client: load_client rejects
        // and list_clients skips, but the row stays on disk.
        save_client(&ClientEntry {
            name: "gone".into(),
            disabled: true,
            ..entry.clone()
        })
        .unwrap();
        assert!(load_client("gone").is_err());
        let mut lo_disabled = lo.clone();
        lo_disabled.disabled = true;
        save_client(&lo_disabled).unwrap();
        assert!(load_client("localhost").is_err());
        assert_eq!(list_clients().unwrap(), vec!["box-1"]);

        // rm = reset to factory: delete the row, next load regenerates
        // the defaults (also how a disabled localhost comes back).
        remove("clients", "localhost").unwrap();
        let lo3 = load_client("localhost").unwrap();
        assert!(lo3.local && !lo3.disabled);
        assert!(tmp.join("clients/localhost.toml").exists());
        std::fs::remove_dir_all(&tmp).ok();
    }
}
