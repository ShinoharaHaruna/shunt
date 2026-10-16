//! shunt: FIFO + priority-band + at-least-once experiment queue for
//! hourly-billed cloud GPU boxes. One binary runs on both sides:
//! server subcommands operate the registry and spool on the dev
//! machine; agent subcommands operate the local queue on a rented box.
//!
//! Behavioral spec: docs/design.md.

pub mod agent;
pub mod cli;
pub mod proto;
pub mod server;
