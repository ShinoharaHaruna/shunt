//! Command handlers — one file per command group. `cli::run` is a
//! pure dispatch table over these; each handler owns one Command
//! variant's flags and surfaces (server wire, local queue, or
//! dual-mode via `cli::dual_mode`).

mod client;
mod control;
mod enqueue;
mod misc;
mod observe;
mod project;
mod queue;
mod tasks;

pub(super) use client::client;
pub(super) use control::{cancel, clean, kill, prio, shutdown, workers};
pub(super) use enqueue::{enqueue, mktask, new};
pub(super) use misc::{completions, dispatch, probe, serve, tunnel, version};
pub(super) use observe::{cat, dlog, logs, status};
pub(super) use project::project;
pub(super) use queue::{daemon, init, restart, start, stop};
pub(super) use tasks::{spool, submit, task};
