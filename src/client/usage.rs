//! Per-task resource telemetry (design.md §3.16). The daemon takes
//! one `Snapshot` per poll cycle and folds it into each running
//! task's running PEAK accumulator; the peaks land as a `usage`
//! sidecar inside the entry dir, so `status` reads the same file
//! for live and terminal tasks alike. Declared estimates
//! (`# shunt:gpu_mem_mb=` etc., §3.13) are annotation only — nothing
//! gates on them; the agent refines the next estimate against the
//! observed diff.

use crate::client::queue;
pub use crate::proto::wire::{Declared, GpuState, Resources, TaskUsage};
use anyhow::Result;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

fn smi(args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("nvidia-smi").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Per-GPU inventory + live counters (index/uuid/name/total/free/
/// util). `None` fields inside a GpuState mean that column didn't
/// parse — the card still lists.
pub fn gpus() -> Vec<GpuState> {
    let Some(text) = smi(&["--query-gpu=index,uuid,name,memory.total,memory.free,utilization.gpu", "--format=csv,noheader,nounits"]) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| {
            let mut p = l.splitn(6, ',').map(|s| s.trim());
            Some(GpuState { index: p.next()?.parse().ok()?, uuid: p.next().unwrap_or("").to_string(), name: p.next().unwrap_or("").to_string(), mem_total_mb: p.next().and_then(|s| s.parse().ok()), mem_free_mb: p.next().and_then(|s| s.parse().ok()), util_pct: p.next().and_then(|s| s.parse().ok()) })
        })
        .collect()
}

/// Declared estimates for one entry — its task.sh comment head.
/// Read at status time; pending entries have no `usage` yet, so
/// the declared half is what makes their detail worth showing.
pub fn declared(root: &Path, track: &str, base: &str) -> Result<Declared> {
    let td = crate::client::exec::task_directives(&queue::task_file(root, track, base));
    Ok(Declared { gpu_mem_mb: td.gpu_mem_mb, ram_mb: td.ram_mb, cpu_cores: td.cpu_cores, retry: td.retry })
}

/// Host RAM + load for the resources block. Host `/proc/meminfo`
/// deliberately, not the cgroup view (§8): on a shared box the
/// contention an agent cares about is foreign use of HOST memory.
pub fn system_resources() -> Resources {
    let mut r = Resources { gpus: gpus(), ..Default::default() };
    if let Ok(text) = std::fs::read_to_string("/proc/meminfo") {
        for l in text.lines() {
            if let Some(v) = l.strip_prefix("MemTotal:") {
                r.ram_total_mb = v.split_whitespace().next().and_then(|s| s.parse::<u64>().ok()).map(|kb| kb / 1024);
            } else if let Some(v) = l.strip_prefix("MemAvailable:") {
                r.ram_free_mb = v.split_whitespace().next().and_then(|s| s.parse::<u64>().ok()).map(|kb| kb / 1024);
            }
        }
    }
    r.load1 = std::fs::read_to_string("/proc/loadavg").ok().and_then(|s| s.split_whitespace().next()?.parse().ok());
    r
}

/// pid → (pgid, utime+stime jiffies, rss bytes), one /proc sweep.
/// `/proc/<pid>/stat` carries all three: after the last `)` (comm
/// may hold spaces/parens) fields are 0=state 1=ppid 2=pgrp …
/// 11=utime 12=stime … 21=rss (pages, 4 KiB).
struct ProcStat {
    pgid: u32,
    jiffies: u64,
    rss_b: u64,
}

fn proc_scan() -> HashMap<u32, ProcStat> {
    let mut out = HashMap::new();
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return out;
    };
    for ent in rd.flatten() {
        let Some(pid) = ent.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(ent.path().join("stat")) else {
            continue; // raced exit; not ours
        };
        let Some(close) = text.rfind(')') else {
            continue;
        };
        let f: Vec<&str> = text[close + 2..].split(' ').collect();
        let (Some(pgid), Some(utime), Some(stime)) = (f.get(2).and_then(|s| s.parse::<u32>().ok()), f.get(11).and_then(|s| s.parse::<u64>().ok()), f.get(12).and_then(|s| s.parse::<u64>().ok())) else {
            continue;
        };
        let rss_pages: i64 = f.get(21).and_then(|s| s.parse().ok()).unwrap_or(0);
        out.insert(pid, ProcStat { pgid, jiffies: utime + stime, rss_b: rss_pages.max(0) as u64 * 4096 });
    }
    out
}

/// pid → (gpu_index, used_mb) for every compute app nvidia-smi
/// sees, with uuid translated to the index form status reports.
fn gpu_apps(uuid_to_idx: &HashMap<String, u32>) -> Vec<(u32, u32, u64)> {
    let Some(text) = smi(&["--query-compute-apps=pid,gpu_uuid,used_gpu_memory", "--format=csv,noheader,nounits"]) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| {
            let mut p = l.splitn(3, ',').map(|s| s.trim());
            let pid = p.next()?.parse::<u32>().ok()?;
            // A uuid the query-gpu map doesn't know (MIG instance,
            // driver drift) is skipped, never guessed onto index 0.
            let idx = *uuid_to_idx.get(p.next()?)?;
            let mb = p.next().and_then(|s| s.parse::<u64>().ok())?;
            Some((pid, idx, mb))
        })
        .collect()
}

/// pgid-aggregated state for one poll cycle — built once, applied
/// to every tracked task. VRAM attribution goes through compute-app
/// pids mapped back to their process groups.
pub struct Snapshot {
    /// pgid → (rss bytes Σ, jiffies Σ)
    groups: HashMap<u32, (u64, u64)>,
    /// pgid → gpu_index → vram bytes Σ
    vram: HashMap<u32, BTreeMap<u32, u64>>,
}

impl Snapshot {
    pub fn take() -> Snapshot {
        let procs = proc_scan();
        let pid_pgid: HashMap<u32, u32> = procs.iter().map(|(&pid, s)| (pid, s.pgid)).collect();
        let uuid_to_idx: HashMap<String, u32> = gpus().into_iter().map(|g| (g.uuid, g.index)).collect();
        let mut groups: HashMap<u32, (u64, u64)> = HashMap::new();
        for s in procs.values() {
            let e = groups.entry(s.pgid).or_default();
            e.0 += s.rss_b;
            e.1 += s.jiffies;
        }
        let mut vram: HashMap<u32, BTreeMap<u32, u64>> = HashMap::new();
        for (pid, idx, mb) in gpu_apps(&uuid_to_idx) {
            if let Some(&pgid) = pid_pgid.get(&pid) {
                *vram.entry(pgid).or_default().entry(idx).or_default() += mb * 1024 * 1024;
            }
        }
        Snapshot { groups, vram }
    }

    /// Fold this cycle's reading into a task's peak accumulator.
    /// `last_jiffies` tracks Σ group jiffies at the previous sample
    /// so cpu_seconds accrues only the delta — group members join
    /// and leave, so per-pid baselines don't work.
    pub fn apply(&self, pgid: u32, u: &mut TaskUsage, last_jiffies: &mut u64) {
        if let Some(&(rss_b, jiffies)) = self.groups.get(&pgid) {
            u.peak_ram_mb = u.peak_ram_mb.max(rss_b / 1024 / 1024);
            // saturating_sub: a fresh group's first sample counts
            // its whole lifetime — that's the right reading for a
            // task that just started accumulating anyway.
            u.cpu_seconds += jiffies.saturating_sub(*last_jiffies) as f64 / 100.0; // CLK_TCK
            *last_jiffies = jiffies;
        }
        if let Some(per_gpu) = self.vram.get(&pgid) {
            for (idx, &bytes) in per_gpu {
                let mb = bytes / 1024 / 1024;
                let e = u.peak_vram_mb.entry(*idx).or_default();
                *e = (*e).max(mb);
            }
        }
        u.samples += 1;
    }
}

/// Persist `usage` inside an entry dir (tmp+rename: a torn read
/// must never hand `status` half a JSON object). Called for
/// running/ entries every cycle; the file then travels with the
/// entry to done//failed/ at finalize for free.
pub fn write_usage(root: &Path, track: &str, base: &str, u: &TaskUsage) {
    let entry = root.join(track).join(base);
    if !entry.is_dir() {
        return;
    }
    let Ok(j) = serde_json::to_string(u) else {
        return;
    };
    let _ = crate::server::fsutil::atomic_write_umask(&entry.join("usage"), j);
}

/// Read an entry's `usage` sidecar (status path). Missing or
/// unparseable → `None` — a torn or legacy entry degrades to
/// "no usage data", never an error that breaks status.
pub fn read_usage(root: &Path, track: &str, base: &str) -> Option<TaskUsage> {
    let text = std::fs::read_to_string(root.join(track).join(base).join("usage")).ok()?;
    serde_json::from_str(&text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// /proc parsing must survive comm values containing spaces and
    /// parentheses — the rfind(')') anchor, not a naive split.
    #[test]
    fn stat_parse_handles_weird_comm() {
        // (weird (comm)) shape: fields after last ')' —
        // state ppid pgrp ... utime stime ... rss
        let line = "1234 (weird (comm)) S 1 999 888 0 -1 4194304 100 0 0 0 60 40 0 0 20 0 1 0 50 12345678 512 18446744073709551615 1 1 0 0 0 0";
        // not calling proc_scan directly (host-dependent); emulate
        // its parse path on this string.
        let close = line.rfind(')').unwrap();
        let f: Vec<&str> = line[close + 2..].split(' ').collect();
        assert_eq!(f[0], "S");
        assert_eq!(f[2].parse::<u32>().unwrap(), 999); // pgrp
        assert_eq!(f[11].parse::<u64>().unwrap(), 60); // utime
        assert_eq!(f[12].parse::<u64>().unwrap(), 40); // stime
        assert_eq!(f[21].parse::<i64>().unwrap(), 512); // rss pages
    }

    /// Usage sidecar round-trips through the entry dir and a torn /
    /// missing file reads as absent, never an error.
    #[test]
    fn usage_file_roundtrip_and_missing() {
        let root = std::env::temp_dir().join(format!("shunt-usage-{}", std::process::id()));
        let entry = root.join("running").join("0100_x");
        std::fs::create_dir_all(&entry).unwrap();
        assert!(read_usage(&root, "running", "0100_x").is_none());
        let mut u = TaskUsage { peak_ram_mb: 512, cpu_seconds: 3.5, samples: 7, ..Default::default() };
        u.peak_vram_mb.insert(0, 2634);
        write_usage(&root, "running", "0100_x", &u);
        let back = read_usage(&root, "running", "0100_x").unwrap();
        assert_eq!(back.peak_vram_mb[&0], 2634);
        assert_eq!(back.peak_ram_mb, 512);
        assert_eq!(back.samples, 7);
        // Corrupt file -> None, not a parse error bubbling up.
        std::fs::write(entry.join("usage"), "{half").unwrap();
        assert!(read_usage(&root, "running", "0100_x").is_none());
        std::fs::remove_dir_all(&root).ok();
    }
}
