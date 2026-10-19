//! Wire protocol types (design.md §5). Every client subcommand answers
//! `--json` with one JSON object carrying `"protocol": 1`.

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::collections::BTreeMap;

/// Wire protocol version, embedded in every JSON response.
pub const PROTOCOL_VERSION: u32 = 1;

/// Extract the wire response from remote stdout that may carry
/// login-shell noise — `ssh <box> 'cmd'` runs the user's shell, so
/// `.bashrc` echoes, motd, or conda-init banners can precede the
/// answer. Responses are one compact JSON object line (`jprint`
/// serializes with `to_string`); scan lines from the end and take
/// the first that parses as an object carrying `"protocol"`, then
/// check that field against `PROTOCOL_VERSION` — a wrong version or
/// a JSON answer with no `protocol` at all means the box's binary
/// drifted from this build (design.md §5), and the error says so.
pub fn extract(out: &str) -> anyhow::Result<serde_json::Value> {
    let mut saw_object = false;
    for line in out.lines().rev() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            if !v.is_object() {
                continue;
            }
            saw_object = true;
            if let Some(p) = v.get("protocol") {
                let n = p.as_u64().unwrap_or(u64::MAX);
                anyhow::ensure!(
                    n == u64::from(PROTOCOL_VERSION),
                    "wire protocol mismatch: remote answered protocol={p}, this build speaks {PROTOCOL_VERSION} \
                     — the box's shunt binary is stale; re-run `shunt client bootstrap <name>` to push the current binary"
                );
                return Ok(v);
            }
        }
    }
    if saw_object {
        anyhow::bail!(
            "remote answered JSON without a \"protocol\" field — its shunt binary predates wire v{PROTOCOL_VERSION}; \
             re-run `shunt client bootstrap <name>` to push the current binary"
        );
    }
    anyhow::bail!("no wire JSON object in remote output")
}

/// `extract` + deserialize into a concrete wire type.
pub fn parse<T: DeserializeOwned>(out: &str) -> anyhow::Result<T> {
    let v = extract(out)?;
    Ok(serde_json::from_value(v)?)
}

/// Generic success/error envelope for ops that return small results.
#[derive(Debug, Serialize, Deserialize)]
pub struct WireResult {
    pub protocol: u32,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Op-specific payload (name, pgid, from/to, ...).
    #[serde(flatten)]
    pub fields: BTreeMap<String, serde_json::Value>,
}

impl WireResult {
    pub fn ok(fields: BTreeMap<String, serde_json::Value>) -> Self {
        Self { protocol: PROTOCOL_VERSION, ok: true, error: None, fields }
    }

    pub fn err(msg: impl Into<String>) -> Self {
        Self { protocol: PROTOCOL_VERSION, ok: false, error: Some(msg.into()), fields: BTreeMap::new() }
    }
}

/// `shunt enqueue -` ack (design.md §4.5 step 3).
#[derive(Debug, Serialize, Deserialize)]
pub struct EnqueueResult {
    pub protocol: u32,
    pub ok: bool,
    /// Installed pending name, e.g. `0103_train-v2-ab12.sh`.
    pub name: String,
}

/// `shunt status --json` on a client (design.md §5).
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct StatusReport {
    pub protocol: u32,
    pub daemon: DaemonState,
    pub workers: WorkersState,
    pub running: Vec<String>,
    pub pending: Vec<String>,
    pub done_recent: Vec<ExitRecord>,
    pub failed_recent: Vec<ExitRecord>,
    pub stop: bool,
    pub shutdown: ShutdownState,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct DaemonState {
    pub alive: bool,
    pub pid: Option<u32>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct WorkersState {
    /// Resolved pool size; never a hardcoded literal (design.md §3.8).
    pub value: u32,
    /// Where it came from: "file" | "env" | "default".
    pub source: String,
}

/// A finished task as reported by `status`.
#[derive(Debug, Serialize, Deserialize)]
pub struct ExitRecord {
    /// Task filename, e.g. `0103_train-v2.sh`.
    pub name: String,
    pub rc: i32,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ShutdownState {
    pub armed: bool,
    pub grace_s: u64,
    /// Command that would fire (file > $SHUTDOWN_CMD > default), so an
    /// agent can see the real poweroff path before arming.
    #[serde(default)]
    pub cmd: String,
}

/// `shunt probe --json` machine facts (design.md §8). Fields are
/// cgroup-aware; `nproc`/`free` values are marked suspect, never trusted.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ProbeReport {
    pub protocol: u32,
    /// GPU name; `None` is a valid CPU-only boot (AutoDL no-card).
    pub gpu: Option<String>,
    pub driver: Option<String>,
    pub cuda: Option<String>,
    pub cores: Option<u32>,
    pub mem_gb: Option<f64>,
    pub disk_free_gb: Option<f64>,
    pub python: Option<String>,
    /// `id -u == 0` or passwordless `sudo -n` — "can this box do root
    /// things" (install packages, poweroff). Advisory registry fact.
    #[serde(default)]
    pub can_root: Option<bool>,
    /// Best-guess poweroff command (systemctl/shutdown/poweroff by
    /// availability). Advisory — feeds `shutdown on` arm when the
    /// row's `shutdown_cmd` field is unset.
    #[serde(default)]
    pub shutdown_cmd: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_clean_line() {
        let v = extract("{\"protocol\":1,\"ok\":true}\n").unwrap();
        assert_eq!(v["ok"], true);
    }

    #[test]
    fn extract_skips_login_noise() {
        // banner / motd / conda-init lines before AND after the answer
        let out = "Welcome to AutoDL\n>>> conda initialize >>>\n{\"protocol\":1,\"ok\":true,\"name\":\"x.sh\"}\ntrailing junk\n";
        let v: EnqueueResult = parse(out).unwrap();
        assert_eq!(v.name, "x.sh");
    }

    #[test]
    fn extract_ignores_json_looking_noise() {
        // a banner line that happens to be JSON but is not wire
        let out = "{\"tips\":\"buy more gpu\"}\n{\"protocol\":1,\"ok\":false,\"error\":\"no\"}\n";
        let v = extract(out).unwrap();
        assert_eq!(v["error"], "no");
    }

    #[test]
    fn extract_no_wire_line() {
        assert!(extract("just a banner\nnot json at all\n").is_err());
        assert!(extract("").is_err());
    }

    #[test]
    fn extract_protocol_mismatch_is_named() {
        // A newer (or older) wire version must fail loudly, not be
        // silently consumed — the error steers to the upgrade path.
        let err = extract("{\"protocol\":2,\"ok\":true}\n").unwrap_err().to_string();
        assert!(err.contains("protocol mismatch"), "{err}");
        assert!(err.contains("client bootstrap"), "{err}");
        // Non-numeric protocol counts as mismatch, not as valid.
        assert!(extract("{\"protocol\":\"one\"}\n").is_err());
    }

    #[test]
    fn extract_missing_protocol_names_binary_drift() {
        // JSON without `protocol` is a pre-v1 remote binary, not a
        // format error — the error distinguishes it from noise.
        let err = extract("banner\n{\"ok\":true}\n").unwrap_err().to_string();
        assert!(err.contains("protocol"), "{err}");
        assert!(err.contains("client bootstrap"), "{err}");
    }
}
