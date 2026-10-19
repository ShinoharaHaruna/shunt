//! Registry row types (serde surface — keep structs with their
//! derives/attrs).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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
    /// `<qroot>/shutdown_cmd` by `shutdown -c <name> on`; precedence
    /// for the pushed value is `--cmd` flag > this field > the
    /// `facts.shutdown_cmd` probe guess (design.md §3.11). Point it
    /// at `true`/a marker script in test environments.
    #[serde(default)]
    pub shutdown_cmd: Option<String>,
    /// Billing lifetime: "unlimited" (owned box, localhost, lab GPU),
    /// "metered" (pay-as-you-go — runs until the balance does), or an
    /// ISO-8601 timestamp string for a subscription deadline
    /// ("2026-01-20T18:00:00+08:00"; with an offset, `status` can
    /// report seconds remaining). Empty = unknown.
    #[serde(default)]
    pub expire: String,
    /// Shell snippet sourced before every task on this box
    /// (conda activate, `source env.sh`, PATH exports — whatever the
    /// first manual run proved necessary). Pushed to
    /// `<qroot>/env_setup` by `client env`/`bootstrap`; the runner
    /// sources it in the task wrapper (design.md §3.12).
    #[serde(default)]
    pub env_setup: String,
    /// Shell the task wrapper + env_setup run under on this box
    /// ("bash", "zsh", "sh"). Empty = bash. Pushed to
    /// `<qroot>/shell`. conda-activate-style snippets need bash/zsh,
    /// not POSIX sh — record what the box actually has.
    #[serde(default)]
    pub shell: String,
    /// Whether the vendor reassigns IP/port across stop/start
    /// (AutoDL does not). Drives the escalation text when connection
    /// failures streak — see `CONN_FAIL_ESCALATE`.
    #[serde(default)]
    pub endpoint_rotates: bool,
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
    /// Password for the one-time `ssh-copy-id` key deploy
    /// (`sshpass`). AutoDL boxes default to password auth, so this is
    /// how `bootstrap` installs the generated key unattended. Empty
    /// = no password path; deploy must happen via console or a manual
    /// ssh-copy-id.
    #[serde(default)]
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Billing {
    pub hourly: f64,
    pub currency: String,
}

/// One project; many-to-many with clients via `[[bind]]`.
/// `repo`/`desc` document where the code lives and what it is —
/// agents read the row to pick the right project; `[defaults]`
/// merge under every binding at submit time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectEntry {
    pub name: String,
    #[serde(default)]
    pub desc: String,
    /// Canonical checkout on the dev machine (path or git URL) —
    /// where the code lives for edits; a future rsync source.
    #[serde(default)]
    pub repo: String,
    #[serde(default)]
    pub defaults: ProjectDefaults,
    #[serde(default)]
    pub bind: Vec<Binding>,
}

/// Project-wide defaults, merged under each binding (binding wins).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ProjectDefaults {
    #[serde(default)]
    pub env: BTreeMap<String, String>,
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
