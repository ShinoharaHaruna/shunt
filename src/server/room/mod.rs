//! Coordination rooms (design.md §17): a long-lived named group
//! chat whose members are the PROJECTS sharing compute — the side
//! channel the queue deliberately withholds. Everything is
//! server-local under `$SHUNT_HOME/rooms/<name>/`: no wire op, no
//! box-side state, no interaction with scheduling or the
//! idle-shutdown gate. `meta.json` holds the roster + the current
//! decision; `msgs/NNNN.json` is one linear stream minted under
//! `.seq.lock`; `readers/<label>.json` is each identity's
//! high-water mark.
//!
//! Module map: `store` = dirs/meta/seq/readers fs layer, `labels`
//! = the `--as` identity rules, `create`/`msgs`/`compact`/`ops` =
//! the user-facing ops.

use serde::{Deserialize, Serialize};

mod compact;
mod create;
mod labels;
mod msgs;
mod ops;
mod store;
#[cfg(test)]
mod tests;

pub use compact::{compact_commit, compact_plan, CompactPlan};
pub use create::create;
pub(crate) use labels::{required_label, resolve_label};
pub use msgs::{read, say, ReadMode};
pub use ops::{archive, decide, list, remove, roster, status_rooms, unarchive, Listings};

/// body_md hard cap (§17.1): a coordination message has no business
/// carrying logs — oversized bodies are rejected before any write.
pub const MAX_BODY: usize = 16 * 1024;

/// Live-stream cap (§17.4): past this many `msgs/NNNN.json` the
/// oldest move to `msgs/.archive/` — dot-prefixed, so `read`/`ls`
/// never see them. `SHUNT_ROOM_KEEP` overrides (tests).
pub(super) fn msg_keep() -> usize {
    std::env::var("SHUNT_ROOM_KEEP").ok().and_then(|s| s.parse().ok()).unwrap_or(500)
}

/// Compaction floor: a digest over a short stream saves nothing —
/// plan refuses below this many digestible live messages and points
/// at the manual `--through` escape. `SHUNT_ROOM_COMPACT_MIN`
/// overrides (tests).
pub(super) fn compact_min() -> usize {
    std::env::var("SHUNT_ROOM_COMPACT_MIN").ok().and_then(|s| s.parse().ok()).unwrap_or(100)
}

/// readers/ file cap (§17.4): oldest-mtime evicted past it — a
/// label gone quiet loses only its read positions, never data.
/// `SHUNT_ROOM_READERS_MAX` overrides (tests).
pub(super) fn readers_max() -> usize {
    std::env::var("SHUNT_ROOM_READERS_MAX").ok().and_then(|s| s.parse().ok()).unwrap_or(64)
}

/// Room record: `meta.json` (§17.1). `members` are project names —
/// the shared vocabulary — while `clients` is annotation context
/// (which boxes the room coordinates), never a gate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub created_at: String,
    #[serde(default)]
    pub members: Vec<String>,
    #[serde(default)]
    pub clients: Vec<String>,
    /// The current agreed state; `decide` overwrites it, latest wins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<Decision>,
    /// Compaction ratchet: the highest N covered by a committed
    /// digest. Lives in meta, not derivable from the stream — the
    /// 500-cap can archive the newest summary itself, taking the
    /// record of what it covered with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compacted_through: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    pub ts: String,
    pub author: String,
    pub note: String,
}

/// One stream entry: `msgs/NNNN.json`. `member: false` marks a
/// post from outside the roster — allowed (open trust), but loud.
/// `summary_through` presence alone marks an agent-written
/// compaction digest; its value is the highest covered N (no
/// separate `kind` field — old binaries just ignore it).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Msg {
    pub author: String,
    pub ts: String,
    pub body_md: String,
    pub member: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_through: Option<u32>,
}

/// `readers/<label>.json`: the label's high-water mark (§17.5).
#[derive(Debug, Default, Serialize, Deserialize)]
pub(super) struct Reader {
    #[serde(default)]
    pub(super) last_read_n: u32,
}

/// One suggested room: projects whose bindings land on the same box
/// (client rows sharing an ssh.host:port; local rows group together)
/// with ≥2 distinct projects (§17.4). Computed live from bindings —
/// zero stored state, always accurate.
#[derive(Debug, Serialize)]
pub struct Suggested {
    /// `host:port` for ssh rows, "local" for the loopback group.
    pub endpoint: String,
    pub clients: Vec<String>,
    pub projects: Vec<String>,
}

/// What `ls` reports per room.
#[derive(Debug, Serialize)]
pub struct RoomView {
    pub name: String,
    pub created_at: String,
    pub members: Vec<String>,
    /// Members whose project no longer shares any same-box group —
    /// drift is flagged, never hidden (§17.4).
    pub stale: Vec<String>,
    pub clients: Vec<String>,
    pub msgs: usize,
    /// Unread for the caller's label; `None` when no label resolved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unread_msgs: Option<usize>,
    pub decision: Option<Decision>,
}

/// `admin` (case-insensitive in every comparison) is the human
/// operator's reserved identity (§17.3). No `--as` means the
/// operator — an agent forgetting `--as` posts as a person, not
/// under a stray $USER. `admin` is exempt from the non-member
/// warning (a human reading a foreign room is overseeing, not
/// leaking) yet still records `member: false` — it is honestly not
/// on the roster. It can never BECOME a roster member or a
/// registered project: both are rejected at intake, so no lookup
/// can turn the operator into a project.
pub const ADMIN_LABEL: &str = "admin";

pub(super) fn is_admin(label: &str) -> bool {
    label.eq_ignore_ascii_case(ADMIN_LABEL)
}

/// Roster membership is case-insensitive (§17.3): canonicalization
/// already aligns registered project names, and the insensitive
/// compare extends the same courtesy to rosters holding
/// non-registered member names.
pub(super) fn on_roster(meta: &Meta, label: &str) -> bool {
    meta.members.iter().any(|m| m.eq_ignore_ascii_case(label))
}

/// Open trust: a non-member op still lands — but name the roster so
/// a wrong-room typo is loud, not silently leaked into a foreign
/// channel (§17.3). `admin` is exempt: the operator is the one
/// reader the warning would be noise for.
pub(super) fn warn_non_member(name: &str, meta: &Meta, label: &str, action: &str) {
    if !is_admin(label) && !on_roster(meta, label) {
        eprintln!("warning: '{label}' is not a member of room '{name}' (roster: {}) — {action}", meta.members.join(", "));
    }
}
