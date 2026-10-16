//! Wire protocol types (design.md §5). Every client subcommand answers
//! `--json` with one JSON object carrying `"protocol": 1`.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Wire protocol version, embedded in every JSON response.
pub const PROTOCOL_VERSION: u32 = 1;

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
        Self {
            protocol: PROTOCOL_VERSION,
            ok: true,
            error: None,
            fields,
        }
    }

    pub fn err(msg: impl Into<String>) -> Self {
        Self {
            protocol: PROTOCOL_VERSION,
            ok: false,
            error: Some(msg.into()),
            fields: BTreeMap::new(),
        }
    }
}

/// `shunt enqueue -` ack (design.md §4.3 step 3).
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
    pub done_recent: Vec<String>,
    pub failed_recent: Vec<String>,
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

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ShutdownState {
    pub armed: bool,
    pub grace_s: u64,
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
}
