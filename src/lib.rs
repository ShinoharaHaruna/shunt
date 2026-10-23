//! shunt: experiment operations for local and remote CPU or GPU machines.
//! One binary manages the dev-side registry, spool, and coordination rooms,
//! and runs independent FIFO + priority-band + at-least-once queues on
//! experiment machines. Idle shutdown and billing metadata are optional.
//!
//! Behavioral spec: docs/design.md.

pub mod cli;
pub mod client;
pub mod feedback;
pub mod handoff;
pub mod proto;
pub mod server;

/// The canonical upstream GitHub repo (`owner/name`) — the default
/// target for `feedback` issues and `bootstrap` release-asset fetches.
/// Forks override via `server.toml` fields / `SHUNT_*_REPO` env.
pub const UPSTREAM_REPO: &str = "ShinoharaHaruna/shunt";
