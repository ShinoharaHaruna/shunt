//! shunt: FIFO + priority-band + at-least-once experiment queue for
//! hourly-billed cloud GPU boxes. One binary runs on both sides:
//! server subcommands operate the registry and spool on the dev
//! machine; client subcommands operate the local queue on a rented box.
//!
//! Behavioral spec: docs/design.md.

pub mod cli;
pub mod client;
pub mod feedback;
pub mod proto;
pub mod server;

/// The canonical upstream GitHub repo (`owner/name`) — the default
/// target for `feedback` issues and `bootstrap` release-asset fetches.
/// Forks override via `server.toml` fields / `SHUNT_*_REPO` env.
pub const UPSTREAM_REPO: &str = "ShinoharaHaruna/shunt";
