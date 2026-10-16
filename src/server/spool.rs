//! Server-side staging queue (design.md §4.3). Entries persist under
//! `<SHUNT_HOME>/spool/` as `<task-id>.json` + `<task-id>.sh` so a
//! submission survives while the box is powered off.

use crate::proto::task::SpoolEntry;
use crate::server::registry::shunt_home;
use anyhow::Result;
use std::path::PathBuf;

pub fn spool_dir() -> Result<PathBuf> {
    Ok(shunt_home()?.join("spool"))
}

/// Persist a spool entry + rendered task body. Stub: layout only.
pub fn stage(_entry: &SpoolEntry, _body: &str) -> Result<()> {
    anyhow::bail!("spool::stage: not yet implemented")
}

/// Oldest-first listing of staged entries. Stub.
pub fn list() -> Result<Vec<SpoolEntry>> {
    anyhow::bail!("spool::list: not yet implemented")
}

/// Remove an entry after a clean enqueue ack. Stub.
pub fn remove(_task_id: &str) -> Result<()> {
    anyhow::bail!("spool::remove: not yet implemented")
}
