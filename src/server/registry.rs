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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SshEndpoint {
    pub host: String,
    pub port: u16,
    pub user: String,
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

fn save<T: Serialize>(dir: &str, name: &str, value: &T) -> Result<PathBuf> {
    let path = shunt_home()?.join(dir).join(format!("{name}.toml"));
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = toml::to_string_pretty(value)?;
    fs::write(&path, text)?;
    Ok(path)
}

pub fn load_client(name: &str) -> Result<ClientEntry> {
    load("clients", name)
}

pub fn save_client(entry: &ClientEntry) -> Result<PathBuf> {
    save("clients", &entry.name, entry)
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
        std::fs::remove_dir_all(&tmp).ok();
    }
}
