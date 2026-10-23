//! `status`: the wire StatusReport plus its human-readable render.

use anyhow::Result;

use super::{jprint, local_root, wire};
use crate::cli::args::StatusArgs;

pub(crate) fn agent_status(a: &StatusArgs) -> Result<()> {
    wire(a.json, status_impl(a))
}

fn status_impl(a: &StatusArgs) -> Result<()> {
    use crate::proto::wire::{DaemonState, EntryDetail, ExitRecord};
    let root = local_root(&a.root)?;
    let alive = crate::client::daemon::alive(&root)?;
    // The §3.5a capability pair: ver is the restart gate, bin is the
    // diagnosis for a daemon started from a path other than the
    // upgraded one. Read unconditionally — a stale pair on a dead
    // daemon is reported honestly as absent-ish state.
    let sidecar = |f: &str| std::fs::read_to_string(root.join(f)).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let detail = |track: &str, name: &str| -> EntryDetail { EntryDetail { name: name.to_string(), declared: crate::client::usage::declared(&root, track, name).unwrap_or_default(), usage: crate::client::usage::read_usage(&root, track, name) } };
    let pending_names = crate::client::queue::pending_list(&root)?;
    let running_names = crate::client::queue::running_list(&root)?;
    let exit_recs = |track: &'static str| -> Vec<ExitRecord> { crate::client::queue::recent_exits(&root, track, 5).into_iter().map(|(name, rc)| ExitRecord { usage: crate::client::usage::read_usage(&root, track, &name), declared: crate::client::usage::declared(&root, track, &name).unwrap_or_default(), name, rc }).collect() };
    let report = crate::proto::wire::StatusReport { protocol: crate::proto::wire::PROTOCOL_VERSION, daemon: DaemonState { alive: alive.is_some(), pid: alive, ver: sidecar("daemon.ver"), bin: sidecar("daemon.bin") }, workers: crate::client::daemon::resolve_workers(&root), running: running_names.clone(), pending: pending_names.clone(), done_recent: exit_recs("done"), failed_recent: exit_recs("failed"), stop: root.join("STOP").exists(), shutdown: crate::client::shutdown::status(&root)?, resources: Some(crate::client::usage::system_resources()), pending_detail: pending_names.iter().map(|n| detail("pending", n)).collect(), running_detail: running_names.iter().map(|n| detail("running", n)).collect(), needs_glibc: crate::proto::libc::self_glibc_need().map(|(a, b)| format!("{a}.{b}")) };
    if a.json {
        jprint(&report);
        return Ok(());
    }
    println!("== daemon ==");
    match report.daemon.pid {
        Some(pid) if report.daemon.alive => println!("  running (pid {pid})"),
        _ => println!("  stopped   (start: shunt start)"),
    }
    println!("  workers: {} ({})", report.workers.value, report.workers.source);
    if report.stop {
        println!("  STOP sentinel present (draining)");
    }
    if let Some(r) = &report.resources {
        println!("== resources ==");
        for g in &r.gpus {
            let free = g.mem_free_mb.map(|m| format!("{:.1}", m as f64 / 1024.0)).unwrap_or_else(|| "?".into());
            let total = g.mem_total_mb.map(|m| format!("{:.1}", m as f64 / 1024.0)).unwrap_or_else(|| "?".into());
            let util = g.util_pct.map(|u| format!("{u}%")).unwrap_or_else(|| "?".into());
            println!("  gpu{} {}: {free}/{total} GiB free, {util} util", g.index, g.name);
        }
        if r.ram_total_mb.is_some() || r.ram_free_mb.is_some() || r.load1.is_some() {
            let free = r.ram_free_mb.map(|m| format!("{:.1}", m as f64 / 1024.0)).unwrap_or_else(|| "?".into());
            let total = r.ram_total_mb.map(|m| format!("{:.1}", m as f64 / 1024.0)).unwrap_or_else(|| "?".into());
            let load = r.load1.map(|l| format!("{l:.2}")).unwrap_or_else(|| "?".into());
            println!("  ram: {free}/{total} GiB free   load1 {load}");
        }
    }
    let annotate = |d: &EntryDetail| -> String {
        let mut bits = Vec::new();
        if let Some(m) = d.declared.gpu_mem_mb {
            bits.push(format!("declared vram {m} MiB"));
        }
        if let Some(u) = &d.usage {
            if let Some(v) = u.peak_vram_mb.values().max() {
                bits.push(format!("peak vram {v} MiB"));
            }
            if u.peak_ram_mb > 0 {
                bits.push(format!("peak ram {} MiB", u.peak_ram_mb));
            }
        }
        if bits.is_empty() {
            String::new()
        } else {
            format!("  ({})", bits.join(", "))
        }
    };
    println!("== running ==");
    if report.running.is_empty() {
        println!("  (idle)");
    } else {
        for d in &report.running_detail {
            println!("  {}{}", d.name, annotate(d));
        }
    }
    println!("== pending (execution order) ==");
    if report.pending.is_empty() {
        println!("  (empty)");
    } else {
        for d in &report.pending_detail {
            println!("  {}{}", d.name, annotate(d));
        }
    }
    for (label, recs) in [("done", &report.done_recent), ("failed", &report.failed_recent)] {
        println!("== {label} (recent) ==");
        for r in recs {
            let mut line = format!("  {:<48} rc={}", r.name, r.rc);
            if let Some(u) = &r.usage {
                if let Some(v) = u.peak_vram_mb.values().max() {
                    line.push_str(&format!("  peak vram {v} MiB"));
                }
            }
            println!("{line}");
        }
    }
    println!("logs: tail -f {0}/running/<task>/log   (daemon: {0}/daemon.log)", root.display());
    Ok(())
}
