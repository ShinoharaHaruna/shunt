//! Agent-side (client) dispatch helpers. These back the local
//! queue subcommands and double as the SSH wire surface via --json.

use std::path::PathBuf;

use anyhow::{Context as _, Result};

use super::args::{EnqueueArgs, LogsArgs, NewArgs, PatternArgs, PrioArgs, RootArgs, ShutdownAction, ShutdownArgs, StatusArgs, WorkerAction, WorkersArgs};

pub(crate) fn agent_root(flag: &Option<String>) -> Result<PathBuf> {
    crate::agent::queue::resolve_root(flag.as_deref())
}

pub(crate) fn jprint(v: impl serde::Serialize) {
    println!("{}", serde_json::to_string(&v).unwrap_or_default());
}

/// json! object -> field map, for building wire responses.
pub(crate) fn jfields(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    v.as_object().cloned().unwrap_or_default()
}

pub(crate) fn jobj(fields: serde_json::Map<String, serde_json::Value>) -> serde_json::Value {
    let mut o = serde_json::Map::new();
    o.insert("protocol".into(), serde_json::json!(crate::proto::wire::PROTOCOL_VERSION));
    o.insert("ok".into(), serde_json::json!(true));
    o.extend(fields);
    serde_json::Value::Object(o)
}

/// Emit the wire error object and exit nonzero, or propagate the
/// error for the human-facing path.
pub(crate) fn jfail(json: bool, msg: &str) -> Result<()> {
    if json {
        jprint(serde_json::json!({"protocol": crate::proto::wire::PROTOCOL_VERSION,
                                  "ok": false, "error": msg}));
        std::process::exit(1);
    }
    anyhow::bail!("{msg}")
}

/// Wrap a `--json`-capable handler body: expected failures already
/// `jfail`, but a stray `?` propagation still owes wire callers the
/// error object (design.md §5) — stderr-only failures surface on the
/// server as "no wire JSON object" instead of the real error.
fn wire(json: bool, r: Result<()>) -> Result<()> {
    match r {
        Ok(()) => Ok(()),
        Err(e) => jfail(json, &format!("{e:#}")),
    }
}

pub(crate) fn agent_enqueue(a: &EnqueueArgs) -> Result<()> {
    wire(a.json, enqueue_impl(a))
}

fn enqueue_impl(a: &EnqueueArgs) -> Result<()> {
    use std::io::Read;
    let root = agent_root(&a.root)?;
    let inner = || -> Result<crate::agent::queue::EnqueueOutcome> {
        let body = if a.file == "-" {
            let mut b = String::new();
            std::io::stdin().read_to_string(&mut b)?;
            b
        } else {
            std::fs::read_to_string(&a.file).with_context(|| format!("no such file: {}", a.file))?
        };
        let raw_slug = match a.slug.as_ref().or(a.slug_opt.as_ref()) {
            Some(s) => s.clone(),
            None if a.file == "-" => anyhow::bail!("slug required for stdin form"),
            None => std::path::Path::new(&a.file).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
        };
        let slug = crate::proto::task::sanitize_slug(&raw_slug);
        if let Some(p) = crate::proto::task::slug_problem(&slug) {
            anyhow::bail!("bad slug '{raw_slug}': {p}");
        }
        crate::agent::queue::enqueue(&root, &slug, &body)
    };
    match inner() {
        Ok(outcome) => {
            let name = outcome.name();
            if a.json {
                jprint(jobj(jfields(serde_json::json!({"name": name, "deduped": outcome.deduped()}))));
            } else {
                if outcome.deduped() {
                    println!("enqueued: {name} (deduped — this delivery already had an entry)");
                } else {
                    println!("enqueued: {name}");
                }
                if crate::agent::daemon::alive(&root)?.is_none() {
                    eprintln!("note: queue daemon not running; start with `shunt start`");
                }
            }
            Ok(())
        }
        Err(e) => jfail(a.json, &format!("{e:#}")),
    }
}

pub(crate) fn agent_new(a: &NewArgs) -> Result<()> {
    wire(a.json, new_impl(a))
}

fn new_impl(a: &NewArgs) -> Result<()> {
    let json = a.json;
    let inner = || -> Result<crate::agent::queue::EnqueueOutcome> {
        let meta = a.task_meta()?;
        let body = crate::agent::exec::render_task(&meta);
        let root = agent_root(&a.root)?;
        crate::agent::queue::enqueue(&root, &meta.slug, &body)
    };
    match inner() {
        Ok(outcome) => {
            let name = outcome.name();
            if json {
                jprint(jobj(jfields(serde_json::json!({"name": name, "deduped": outcome.deduped()}))));
            } else {
                if outcome.deduped() {
                    println!("enqueued: {name} (deduped — this delivery already had an entry)");
                } else {
                    println!("enqueued: {name}");
                }
                let root = agent_root(&a.root)?;
                if crate::agent::daemon::alive(&root)?.is_none() {
                    eprintln!("note: queue daemon not running; start with `shunt start`");
                }
            }
            Ok(())
        }
        Err(e) => jfail(json, &format!("{e:#}")),
    }
}

/// Reduce a `match_pending`/`match_running` hit list to the one
/// task the op acts on: `none` jfails an empty list, `several` a
/// multi-hit. The wording stays the caller's because the refusals
/// are the CLI/wire surface — `prio` collapses both cases into one
/// count-carrying message and passes it for both.
fn one_match(m: &[String], json: bool, none: &str, several: &str) -> Result<String> {
    if m.len() == 1 {
        return Ok(m[0].clone());
    }
    jfail(json, if m.is_empty() { none } else { several })?;
    unreachable!("jfail exits under --json and bails otherwise")
}

pub(crate) fn agent_cancel(a: &PatternArgs) -> Result<()> {
    wire(a.json, cancel_impl(a))
}

fn cancel_impl(a: &PatternArgs) -> Result<()> {
    let root = agent_root(&a.root)?;
    let m = crate::agent::queue::match_pending(&root, &a.pattern)?;
    let name = one_match(&m, a.json, &format!("no pending task matches '{}'", a.pattern), &format!("pattern matches several tasks, refusing:\n  {}", m.join("\n  ")))?;
    crate::agent::queue::remove_pending(&root, &name)?;
    if a.json {
        jprint(jobj(jfields(serde_json::json!({"name": name}))));
    } else {
        println!("cancelled: {name}");
    }
    Ok(())
}

pub(crate) fn agent_prio(a: &PrioArgs) -> Result<()> {
    wire(a.json, prio_impl(a))
}

fn prio_impl(a: &PrioArgs) -> Result<()> {
    let root = agent_root(&a.root)?;
    if a.n > crate::agent::queue::PRIORITY_BAND_MAX {
        return jfail(a.json, &format!("prio number must be inside the 0000-{:04} priority band, got {:04}", crate::agent::queue::PRIORITY_BAND_MAX, a.n));
    }
    let m = crate::agent::queue::match_pending(&root, &a.pattern)?;
    let miss = format!("need exactly one pending match, got {}:\n  {}", m.len(), m.join("\n  "));
    let base = one_match(&m, a.json, &miss, &miss)?;
    let rest = base.split_once('_').map(|(_, r)| r).unwrap_or(base.as_str());
    let new = format!("{:04}_{rest}", a.n);
    // Entry shapes: a dir renames to a dir; a legacy flat file keeps
    // its `.sh` suffix so it stays visible to the listing.
    let (from, to) = if crate::agent::queue::is_dir_entry(&root, "pending", &base) { (root.join("pending").join(&base), root.join("pending").join(&new)) } else { (root.join("pending").join(format!("{base}.sh")), root.join("pending").join(format!("{new}.sh"))) };
    // refuse to clobber an existing pending task (bash `mv -f` defect,
    // design.md §3.14); check BOTH shapes for BOTH entry shapes — a
    // flat source renaming onto a same-named pending DIR would leave
    // `NNNN_x/` + `NNNN_x.sh` coexisting as two tasks.
    let occupied = root.join("pending").join(&new).exists() || root.join("pending").join(format!("{new}.sh")).exists();
    if occupied && new != base {
        return jfail(a.json, &format!("target name exists: {new}"));
    }
    if new != base {
        if crate::agent::queue::is_dir_entry(&root, "pending", &base) {
            // check-then-rename: rename onto a non-empty dir already
            // fails; onto an EMPTY dir it silently replaces — a
            // residual race window the pre-check above shrinks but
            // cannot close (an empty dir holds no task content).
            std::fs::rename(&from, &to).with_context(|| format!("cannot rename {}", from.display()))?;
        } else {
            // hard_link fails EEXIST atomically — closes the
            // check-then-rename gap where a same-name flat task
            // created in between would be silently clobbered.
            match std::fs::hard_link(&from, &to) {
                Ok(()) => std::fs::remove_file(&from).with_context(|| format!("cannot remove {}", from.display()))?,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return jfail(a.json, &format!("target name exists: {new}")),
                Err(e) => return Err(e).with_context(|| format!("cannot rename {}", from.display())),
            }
        }
    }
    if a.json {
        jprint(jobj(jfields(serde_json::json!({"from": base, "to": new}))));
    } else {
        println!("renamed: {base} -> {new}");
    }
    Ok(())
}

pub(crate) fn agent_kill(a: &PatternArgs) -> Result<()> {
    wire(a.json, kill_impl(a))
}

fn kill_impl(a: &PatternArgs) -> Result<()> {
    let root = agent_root(&a.root)?;
    let m = crate::agent::queue::match_running(&root, &a.pattern)?;
    let name = one_match(&m, a.json, &format!("no running task matches '{}'", a.pattern), &format!("pattern matches several tasks, refusing:\n  {}", m.join("\n  ")))?;
    match crate::agent::exec::kill_task(&root, &name) {
        Ok(crate::agent::exec::KillOutcome::Gone) => {
            // Honest report: the recorded pgid no longer names
            // this task's leader (reaped already or recycled) —
            // nothing was signaled.
            if a.json {
                jprint(jobj(jfields(serde_json::json!({"gone": true}))));
            } else {
                println!("{name} already gone; nothing signaled");
            }
            Ok(())
        }
        Ok(crate::agent::exec::KillOutcome::Signaled { pgid, escalated }) => {
            if a.json {
                jprint(jobj(jfields(serde_json::json!({"pgid": pgid, "escalated": escalated}))));
            } else {
                println!("killing {name} (pgid {pgid})");
                if escalated {
                    println!("still alive after 10 s; sent SIGKILL");
                } else {
                    println!("terminated");
                }
            }
            Ok(())
        }
        Err(e) => jfail(a.json, &format!("{e:#}")),
    }
}

pub(crate) fn agent_workers(a: &WorkersArgs) -> Result<()> {
    wire(a.json, workers_impl(a))
}

fn workers_impl(a: &WorkersArgs) -> Result<()> {
    let root = agent_root(&a.root)?;
    let set_to = match (&a.n, &a.action) {
        (Some(n), _) => Some(*n),
        (None, Some(WorkerAction::Set { n })) => Some(*n),
        _ => None,
    };
    if let Some(n) = set_to {
        // tmp+rename (umask flavor — a qroot is outside §4.1's
        // 0600 scope): a direct truncate+write leaves an empty file
        // mid-write, and a daemon polling then falls back to
        // last-known workers for one cycle.
        crate::server::fsutil::atomic_write_umask(&root.join("workers"), format!("{n}\n"))?;
    }
    let w = crate::agent::daemon::resolve_workers(&root);
    if a.json {
        jprint(jobj(jfields(serde_json::json!({"workers": w.value, "source": w.source}))));
    } else {
        println!("workers: {} ({})", w.value, w.source);
    }
    Ok(())
}

pub(crate) fn agent_shutdown(a: &ShutdownArgs) -> Result<()> {
    wire(a.json, shutdown_impl(a))
}

fn shutdown_impl(a: &ShutdownArgs) -> Result<()> {
    let root = agent_root(&a.root)?;
    let counts = || -> serde_json::Value {
        serde_json::json!({
            "running": crate::agent::queue::running_list(&root).map(|v| v.len()).unwrap_or(0),
            "pending": crate::agent::queue::pending_list(&root).map(|v| v.len()).unwrap_or(0),
        })
    };
    match a.action {
        ShutdownAction::On { grace_s, ref cmd } => {
            if let Some(c) = cmd {
                crate::agent::shutdown::set_cmd(&root, c.as_str())?;
            }
            crate::agent::shutdown::arm(&root, grace_s)?;
            let st = crate::agent::shutdown::status(&root)?;
            if a.json {
                let mut f = serde_json::json!({"armed": true, "grace_s": st.grace_s, "cmd": st.cmd});
                f.as_object_mut().unwrap().extend(counts().as_object().unwrap().clone());
                jprint(jobj(jfields(f)));
            } else {
                println!("armed: shutdown_on_idle set; poweroff fires {}s after pending/ AND running/ are both empty (cmd: {})", st.grace_s, st.cmd);
            }
        }
        ShutdownAction::Off => {
            crate::agent::shutdown::disarm(&root)?;
            if a.json {
                let mut f = serde_json::json!({"armed": false});
                f.as_object_mut().unwrap().extend(counts().as_object().unwrap().clone());
                jprint(jobj(jfields(f)));
            } else {
                println!("disarmed: shutdown_on_idle removed (a running countdown cancels within 1 s)");
            }
        }
        ShutdownAction::Status => {
            let st = crate::agent::shutdown::status(&root)?;
            if a.json {
                let mut f = serde_json::json!({"armed": st.armed, "grace_s": st.grace_s, "cmd": st.cmd});
                f.as_object_mut().unwrap().extend(counts().as_object().unwrap().clone());
                jprint(jobj(jfields(f)));
            } else if st.armed {
                println!("shutdown_on_idle: ARMED (grace {}s, cmd: {})", st.grace_s, st.cmd);
            } else {
                println!("shutdown_on_idle: off   (arm: shunt shutdown on [grace_s])");
            }
            if !a.json {
                let c = counts();
                println!("queue now: {} running, {} pending", c["running"], c["pending"]);
            }
        }
    }
    Ok(())
}

pub(crate) fn agent_status(a: &StatusArgs) -> Result<()> {
    wire(a.json, status_impl(a))
}

fn status_impl(a: &StatusArgs) -> Result<()> {
    use crate::proto::wire::{DaemonState, EntryDetail, ExitRecord};
    let root = agent_root(&a.root)?;
    let alive = crate::agent::daemon::alive(&root)?;
    let detail = |track: &str, name: &str| -> EntryDetail { EntryDetail { name: name.to_string(), declared: crate::agent::usage::declared(&root, track, name).unwrap_or_default(), usage: crate::agent::usage::read_usage(&root, track, name) } };
    let pending_names = crate::agent::queue::pending_list(&root)?;
    let running_names = crate::agent::queue::running_list(&root)?;
    let exit_recs = |track: &'static str| -> Vec<ExitRecord> { crate::agent::queue::recent_exits(&root, track, 5).into_iter().map(|(name, rc)| ExitRecord { usage: crate::agent::usage::read_usage(&root, track, &name), declared: crate::agent::usage::declared(&root, track, &name).unwrap_or_default(), name, rc }).collect() };
    let report = crate::proto::wire::StatusReport { protocol: crate::proto::wire::PROTOCOL_VERSION, daemon: DaemonState { alive: alive.is_some(), pid: alive }, workers: crate::agent::daemon::resolve_workers(&root), running: running_names.clone(), pending: pending_names.clone(), done_recent: exit_recs("done"), failed_recent: exit_recs("failed"), stop: root.join("STOP").exists(), shutdown: crate::agent::shutdown::status(&root)?, resources: Some(crate::agent::usage::system_resources()), pending_detail: pending_names.iter().map(|n| detail("pending", n)).collect(), running_detail: running_names.iter().map(|n| detail("running", n)).collect() };
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

/// Find task logs matching `pat` across running/, done/, failed/ —
/// entry dirs' `log` files plus any flat `*.log` leftovers (legacy
/// sidecars, `<clean>.interrupted.<ts>.log` sweep products).
pub(crate) fn find_logs(root: &std::path::Path, pat: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for track in ["running", "done", "failed"] {
        for name in crate::agent::queue::list_entries(root, track).unwrap_or_default() {
            if name.contains(pat) {
                let log = crate::agent::queue::piece_file(root, track, &name, "log");
                if log.exists() {
                    out.push(log);
                }
            }
        }
        if let Ok(rd) = std::fs::read_dir(root.join(track)) {
            for ent in rd.flatten() {
                let name = ent.file_name().to_string_lossy().into_owned();
                if ent.file_type().map(|t| t.is_file()).unwrap_or(false) && name.ends_with(".log") && name.contains(pat) {
                    out.push(ent.path());
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Wire argv for a remote `logs`: `logs [-n N] [-- pat]`.
/// The pattern goes only when present — an empty positional would
/// parse as `Some("")` on the far end and match every task log
/// instead of selecting the daemon log. `-f` never crosses: the
/// caller bails on follow before building this.
pub(crate) fn logs_wire_tail(a: &LogsArgs) -> Vec<String> {
    let mut tail: Vec<String> = vec!["logs".into()];
    if let Some(n) = a.tail {
        tail.push("-n".into());
        tail.push(n.to_string());
    }
    if let Some(pat) = &a.pattern {
        // `--` keeps a dash-leading pattern positional on the
        // remote instead of parsing as a flag.
        tail.push("--".into());
        tail.push(pat.clone());
    }
    tail
}

/// Wire argv for a remote `workers`: `workers set N` when either
/// spelling asks for a set (positional `N` or `set {N}` — same
/// resolution as `agent_workers`), else `workers get`. Without
/// this, `workers -c box set 5` would send `get` and report the
/// old value as if the set had landed.
pub(crate) fn workers_wire_tail(a: &WorkersArgs) -> Vec<String> {
    let set_to = match (&a.n, &a.action) {
        (Some(n), _) => Some(*n),
        (None, Some(WorkerAction::Set { n })) => Some(*n),
        _ => None,
    };
    match set_to {
        Some(n) => vec!["workers".into(), "set".into(), n.to_string()],
        None => vec!["workers".into(), "get".into()],
    }
}

/// The last `n` newline-terminated lines of `bytes`. A trailing
/// newline is a terminator, not an empty extra line; fewer than
/// `n` lines returns the whole buffer.
pub(crate) fn tail_bytes(bytes: &[u8], n: usize) -> &[u8] {
    // `tail -n 0` prints nothing; the `seen == n` check below can only
    // fire after `seen += 1`, so n = 0 must early-return here.
    if n == 0 {
        return &[];
    }
    let mut end = bytes.len();
    if end > 0 && bytes[end - 1] == b'\n' {
        end -= 1;
    }
    let mut seen = 0;
    for (i, &b) in bytes[..end].iter().enumerate().rev() {
        if b == b'\n' {
            seen += 1;
            if seen == n {
                return &bytes[i + 1..];
            }
        }
    }
    bytes
}

pub(crate) fn print_file(path: &std::path::Path) {
    use std::io::Write;
    match std::fs::read(path) {
        Ok(bytes) => {
            let _ = std::io::stdout().write_all(&bytes);
        }
        Err(e) => eprintln!("shunt: cannot read {}: {e}", path.display()),
    }
}

pub(crate) fn follow_file(path: &std::path::Path) -> Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::End(0))?;
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let n = f.read_to_end(&mut buf)?;
        if n > 0 {
            use std::io::Write;
            let _ = std::io::stdout().write_all(&buf);
            let _ = std::io::stdout().flush();
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

pub(crate) fn agent_logs(a: &LogsArgs) -> Result<()> {
    let root = agent_root(&a.root)?;
    match &a.pattern {
        None => {
            agent_dlog(&RootArgs { root: a.root.clone(), json: false }, a.tail)?;
            if a.follow {
                follow_dlog(&root.join("daemon.log"))?;
            }
            Ok(())
        }
        Some(pat) => {
            let m = find_logs(&root, pat);
            match m.len() {
                0 => anyhow::bail!("no task log matches '{pat}'"),
                1 => {
                    match a.tail {
                        Some(n) => {
                            use std::io::Write;
                            let bytes = std::fs::read(&m[0])?;
                            let _ = std::io::stdout().write_all(tail_bytes(&bytes, n));
                        }
                        None => print_file(&m[0]),
                    }
                    if a.follow {
                        follow_file(&m[0])?;
                    }
                    Ok(())
                }
                _ => {
                    eprintln!("pattern matches several logs, refusing:");
                    for p in &m {
                        eprintln!("  {}", p.display());
                    }
                    anyhow::bail!("ambiguous log pattern '{pat}'")
                }
            }
        }
    }
}

/// Render one daemon.log JSONL line as `[ts] event k=v`.
/// Non-JSON lines pass through verbatim — the dlog contract for
/// lines written before the format settled.
fn render_dlog_line(line: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(line) {
        Ok(v) if v.is_object() => {
            let ts = v.get("ts").and_then(|t| t.as_str()).unwrap_or("");
            let ev = v.get("event").and_then(|t| t.as_str()).unwrap_or("");
            let rest: Vec<String> = v.as_object().map(|o| o.iter().filter(|(k, _)| k.as_str() != "ts" && k.as_str() != "event").map(|(k, v)| format!("{k}={v}")).collect()).unwrap_or_default();
            format!("[{ts}] {ev} {}", rest.join(" "))
        }
        _ => line.to_string(),
    }
}

/// `tail -f` on daemon.log, rendering each appended line through
/// `render_dlog_line` so `logs -f` looks like `dlog`, not raw JSON.
/// An unterminated tail-of-file fragment is held until its newline
/// arrives so a half-written record never renders mid-line.
fn follow_dlog(path: &std::path::Path) -> Result<()> {
    use std::io::{Read, Seek, SeekFrom, Write};
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::End(0))?;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let mut chunk = Vec::new();
        if f.read_to_end(&mut chunk)? > 0 {
            buf.extend_from_slice(&chunk);
            while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
                let line = String::from_utf8_lossy(&buf[..nl]).into_owned();
                buf.drain(..=nl);
                println!("{}", render_dlog_line(&line));
            }
            let _ = std::io::stdout().flush();
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

pub(crate) fn agent_dlog(a: &RootArgs, tail: Option<usize>) -> Result<()> {
    wire(a.json, dlog_impl(a, tail))
}

fn dlog_impl(a: &RootArgs, tail: Option<usize>) -> Result<()> {
    let root = agent_root(&a.root)?;
    let path = root.join("daemon.log");
    // Render JSONL human-readable: `[ts] event k=v`. `--json` skips
    // the render — daemon.log IS JSONL, so the wire form is the file
    // verbatim (still tail-sliced).
    if let Ok(text) = std::fs::read_to_string(&path) {
        let lines: Vec<&str> = text.lines().collect();
        let skip = tail.map_or(0, |n| lines.len().saturating_sub(n));
        for line in &lines[skip..] {
            if a.json {
                println!("{line}");
            } else {
                println!("{}", render_dlog_line(line));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both the one-shot `dlog` render and `logs -f`'s appended-
    /// line render must go through the same JSONL→text transform
    /// so a followed daemon log reads `[ts] event k=v`, not raw JSON.
    #[test]
    fn render_dlog_line_formats_jsonl() {
        let out = render_dlog_line(r#"{"ts":"12:00","event":"claim","task":"0001_a","pid":7}"#);
        // serde_json::Map is BTreeMap: k=v pairs sort by key.
        assert_eq!(out, "[12:00] claim pid=7 task=\"0001_a\"");
        assert_eq!(render_dlog_line("not json"), "not json");
    }

    #[test]
    fn tail_bytes_keeps_last_n_lines() {
        assert_eq!(tail_bytes(b"a\nb\nc\n", 2), b"b\nc\n");
        assert_eq!(tail_bytes(b"a\nb\nc\n", 5), b"a\nb\nc\n");
        assert_eq!(tail_bytes(b"a\nb\nc", 1), b"c");
        assert_eq!(tail_bytes(b"", 3), b"");
        assert_eq!(tail_bytes(b"one\n", 1), b"one\n");
        // L10 regression: -n 0 must print nothing, not the whole file.
        assert_eq!(tail_bytes(b"a\nb\nc\n", 0), b"");
        assert_eq!(tail_bytes(b"", 0), b"");
    }

    #[test]
    fn logs_wire_tail_forwards_tail_and_pattern() {
        let a = LogsArgs { pattern: Some("train".into()), client: None, queue: None, root: None, follow: false, tail: Some(50) };
        assert_eq!(logs_wire_tail(&a), vec!["logs", "-n", "50", "--", "train"]);
        // Bare `logs` must not send an empty positional — that would
        // match every task log instead of the daemon log.
        let bare = LogsArgs { pattern: None, ..a };
        assert_eq!(logs_wire_tail(&bare), vec!["logs", "-n", "50"]);
    }

    /// `set 5` spelled via the subcommand must cross the wire as
    /// `workers set 5`, not `workers get` (which would report the
    /// old value as a silent no-op).
    #[test]
    fn workers_wire_tail_honors_set_subcommand() {
        let wa = |n, action| WorkersArgs { n, action, client: None, queue: None, root: None, json: false };
        assert_eq!(workers_wire_tail(&wa(None, Some(WorkerAction::Set { n: 5 }))), vec!["workers", "set", "5"]);
        // Positional `N` still sets; `get` and bare both fetch.
        assert_eq!(workers_wire_tail(&wa(Some(3), None)), vec!["workers", "set", "3"]);
        assert_eq!(workers_wire_tail(&wa(None, Some(WorkerAction::Get))), vec!["workers", "get"]);
        assert_eq!(workers_wire_tail(&wa(None, None)), vec!["workers", "get"]);
    }

    /// prio must refuse a flat entry renaming onto a same-named
    /// pending DIR (and a dir onto a flat `.sh`): either coexistence
    /// would list as two pending tasks (design.md §3.14).
    #[test]
    fn prio_refuses_cross_shape_target() {
        let root = std::env::temp_dir().join(format!("shunt-prio-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        let pending = root.join("pending");
        std::fs::create_dir_all(&pending).unwrap();
        // Flat source 0001_a.sh + dir target 0002_a/.
        std::fs::write(pending.join("0001_a.sh"), "echo hi\n").unwrap();
        std::fs::create_dir_all(pending.join("0002_a")).unwrap();
        let args = PrioArgs { pattern: "0001_a".into(), n: 2, client: None, queue: None, root: Some(root.display().to_string()), json: false };
        let err = agent_prio(&args).unwrap_err().to_string();
        assert!(err.contains("target name exists"), "flat->dir not refused: {err}");
        assert!(pending.join("0001_a.sh").exists() && pending.join("0002_a").exists());
        std::fs::remove_dir_all(&root).ok();

        // Dir source 0001_b/ + flat target 0002_b.sh.
        std::fs::create_dir_all(&pending).unwrap();
        std::fs::create_dir_all(pending.join("0001_b")).unwrap();
        std::fs::write(pending.join("0002_b.sh"), "echo hi\n").unwrap();
        let args = PrioArgs { pattern: "0001_b".into(), n: 2, client: None, queue: None, root: Some(root.display().to_string()), json: false };
        let err = agent_prio(&args).unwrap_err().to_string();
        assert!(err.contains("target name exists"), "dir->flat not refused: {err}");
        std::fs::remove_dir_all(&root).ok();
    }
}
