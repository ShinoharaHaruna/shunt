//! Machine facts probe (design.md §8). cgroup-aware: `cpu.max` /
//! `cpu.cfs_quota_us` for cores, `memory.max` / `memory.limit_in_bytes`
//! for memory; `nproc`/`free` only as fallback and marked suspect.
//! Stub.

use crate::proto::wire::ProbeReport;
use anyhow::Result;

/// Collect facts about this box. Stub.
pub fn collect(_deploy_root: &str) -> Result<ProbeReport> {
    anyhow::bail!("probe::collect: not yet implemented")
}
