//! Dispatcher: drains spool/ into clients over SSH (design.md §4.3).
//! Two-phase, at-least-once end to end: push, ack, delete. Auto-heal
//! restarts a dead client daemon before pushing. Stub for now.

use anyhow::Result;

/// Run the dispatch loop. `once` = single sweep, for tests.
pub fn run(once: bool) -> Result<()> {
    anyhow::bail!("dispatch::run(once={once}): not yet implemented")
}
