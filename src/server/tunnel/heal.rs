//! Self-heal: dead recorded tunnels are re-established or their
//! poisoned ALL_PROXY is stripped (design.md §4.3).

use crate::server::registry::ClientEntry;
use crate::server::transport::Transport;
use anyhow::Result;
use std::path::Path;

use super::env::env_write;
use super::liveness::client_target;
use super::state::{client_lock, port_path, recorded_in, recorded_port, status_in, tunnels_dir};
use super::updown::up_locked;

/// What `heal` did for one client whose box answered a probe.
pub enum Heal {
    /// No tunnel was ever set up — nothing to do.
    NotRecorded,
    /// Tunnel ssh is still alive.
    Healthy,
    /// Dead tunnel re-established; carries the new pid.
    Reconnected(u32),
    /// Reconnect failed; ALL_PROXY was stripped from the box's env so
    /// tasks fail over to direct egress instead of a dead proxy. State
    /// files stay — the next sweep retries the reconnect.
    Stripped,
}

/// Self-heal a dead recorded tunnel while the box itself is
/// reachable (design.md §4.3): try `up` once; on failure strip
/// ALL_PROXY so queued tasks are not poisoned by a dead proxy.
/// Called by the dispatcher sweep only on a successful probe — a
/// dead box never reaches this (env cannot be edited anyway).
pub fn heal(t: &dyn Transport, client: &ClientEntry) -> Result<Heal> {
    let dir = tunnels_dir()?;
    let _lock = client_lock(&dir, &client.name)?;
    // Reconnect on the recorded port, not the default: a tunnel
    // brought up with `--port 2080` must self-heal onto 2080 — the
    // port file and the box's ALL_PROXY both name it.
    let port = std::fs::read_to_string(port_path(&dir, &client.name)).ok().and_then(|s| s.trim().parse::<u16>().ok());
    let alive = status_in(&dir, &client.name, client_target(client).as_deref()).alive;
    heal_in(&dir, t, client, alive, |t, c| up_locked(&dir, t, c, port))
}

/// The decision logic, with the state dir, liveness verdict and the
/// reconnect action injected — tests exercise it without
/// `$SHUNT_HOME`, real ssh, or a fake `/proc` cmdline.
pub(super) fn heal_in(dir: &Path, t: &dyn Transport, client: &ClientEntry, alive: bool, reconnect: impl FnOnce(&dyn Transport, &ClientEntry) -> Result<u32>) -> Result<Heal> {
    if !recorded_in(dir, &client.name) {
        return Ok(Heal::NotRecorded);
    }
    if alive {
        // Liveness alone is not health (design.md §4.3): `up` warns
        // and continues when its env write fails, leaving a live
        // tunnel whose box env lacks ALL_PROXY — tasks then bypass
        // the proxy while status claims they route through it.
        // env_write is an idempotent grep-upsert, so re-assert the
        // line for the recorded port every sweep; with no recorded
        // port the right proxy line is unknowable — strip it so
        // tasks fail over to direct egress instead of a phantom
        // proxy (same rationale as the dead path).
        match recorded_port(dir, &client.name) {
            Some(p) => env_write(t, client, "ALL_PROXY", Some(&format!("socks5h://127.0.0.1:{p}")))?,
            None => env_write(t, client, "ALL_PROXY", None)?,
        }
        return Ok(Heal::Healthy);
    }
    match reconnect(t, client) {
        Ok(pid) => Ok(Heal::Reconnected(pid)),
        Err(e) => {
            eprintln!("tunnel: {} dead; reconnect failed ({e:#}); stripping ALL_PROXY from env", client.name);
            // The box answered a probe this sweep, so the strip is
            // expected to work — surface a failure rather than drop it.
            env_write(t, client, "ALL_PROXY", None)?;
            Ok(Heal::Stripped)
        }
    }
}
