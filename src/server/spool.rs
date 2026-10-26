//! Server-side staging queue (design.md §4.4). Entries persist under
//! `<SHUNT_HOME>/spool/` as `<task-id>.json` + `<task-id>.sh` so a
//! submission survives while the box is powered off.

use crate::proto::task::{sanitize_slug, SpoolEntry, TaskFlags, TaskMeta};
use crate::proto::time::{rfc3339_z_now, unix_now};
use crate::server::registry::{self, shunt_home};
use crate::server::transport::shell_quote;
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::PathBuf;

pub fn spool_dir() -> Result<PathBuf> {
    Ok(shunt_home()?.join("spool"))
}

/// Merge the binding's directives into a `--file` task body
/// (design.md §4.4): `# shunt:workdir=` / `# shunt-env:` lines are
/// injected at the top of the comment head so the file's own
/// directives — scanned later by the runner — override them per key
/// (the file is the task author speaking; the binding is context).
/// An explicit `--guard` is injected as a guard line ahead of the
/// first code line; it runs before any file-embedded check, so the
/// flag always wins.
fn merge_file_directives(body: &str, meta: &TaskMeta) -> String {
    let lines: Vec<&str> = body.lines().collect();
    // Directive head starts after a shebang, if any.
    let head_start = usize::from(lines.first().is_some_and(|l| l.starts_with("#!")));
    let head_end = lines.iter().enumerate().skip(head_start).find_map(|(i, l)| if !l.starts_with('#') && !l.trim().is_empty() { Some(i) } else { None }).unwrap_or(lines.len());

    // Directives already present in the file's comment head win over
    // the binding (workdir by key presence, env per key).
    let mut have_workdir = false;
    let mut have_timeout = false;
    let mut have_retry = false;
    let mut have_retry_delay = false;
    let mut have_gpu_mem = false;
    let mut have_ram = false;
    let mut have_cpu_cores = false;
    let mut have_env: std::collections::BTreeSet<String> = Default::default();
    for l in &lines[head_start..head_end] {
        if l.starts_with("# shunt:workdir=") {
            have_workdir = true;
        } else if l.starts_with("# shunt:timeout_s=") {
            have_timeout = true;
        } else if l.starts_with("# shunt:retry=") {
            have_retry = true;
        } else if l.starts_with("# shunt:retry_delay_s=") {
            have_retry_delay = true;
        } else if l.starts_with("# shunt:gpu_mem_mb=") {
            have_gpu_mem = true;
        } else if l.starts_with("# shunt:ram_mb=") {
            have_ram = true;
        } else if l.starts_with("# shunt:cpu_cores=") {
            have_cpu_cores = true;
        } else if let Some(kv) = l.strip_prefix("# shunt-env:") {
            if let Some((k, _)) = kv.split_once('=') {
                have_env.insert(k.trim().to_string());
            }
        }
    }

    let mut inject_head = String::new();
    // The delivery id is always injected above the file's own head,
    // never gated on an existing `# shunt:id=` line: the injected
    // one names THIS delivery, and dedup_key reads the first match,
    // so a stale embedded id can never shadow it.
    if let Some(i) = &meta.id {
        inject_head.push_str(&format!("# shunt:id={i}\n"));
    }
    if !have_workdir {
        if let Some(w) = &meta.workdir {
            inject_head.push_str(&format!("# shunt:workdir={w}\n"));
        }
    }
    if !have_timeout {
        if let Some(t) = meta.timeout_s {
            inject_head.push_str(&format!("# shunt:timeout_s={t}\n"));
        }
    }
    if let Some(n) = meta.retry.filter(|&n| n > 0) {
        if !have_retry {
            inject_head.push_str(&format!("# shunt:retry={n}\n"));
        }
    }
    if let Some(n) = meta.retry_delay_s {
        if !have_retry_delay {
            inject_head.push_str(&format!("# shunt:retry_delay_s={n}\n"));
        }
    }
    if let Some(n) = meta.gpu_mem_mb {
        if !have_gpu_mem {
            inject_head.push_str(&format!("# shunt:gpu_mem_mb={n}\n"));
        }
    }
    if let Some(n) = meta.ram_mb {
        if !have_ram {
            inject_head.push_str(&format!("# shunt:ram_mb={n}\n"));
        }
    }
    if let Some(n) = meta.cpu_cores {
        if !have_cpu_cores {
            inject_head.push_str(&format!("# shunt:cpu_cores={n}\n"));
        }
    }
    for (k, v) in &meta.env {
        if !have_env.contains(k) {
            inject_head.push_str(&format!("# shunt-env:{k}={v}\n"));
        }
    }
    let guard_line = meta.guard.as_ref().map(|g| {
        let q = shell_quote(g);
        // printf %s, not a double-quoted echo: inside `"..."` the
        // single quotes around {q} are literal and a hostile guard's
        // `$( )`/backticks would still expand (same fix as exec.rs).
        format!("if [ -e {q} ]; then printf 'guard hit: %s exists; skipping\\n' {q}; exit 0; fi\n")
    });

    let mut out = String::new();
    for (i, l) in lines.iter().enumerate() {
        if i == head_start {
            out.push_str(&inject_head);
        }
        if i == head_end {
            if let Some(g) = &guard_line {
                out.push_str(g);
            }
        }
        out.push_str(l);
        out.push('\n');
    }
    // Edge cases: comment head runs to (or past) EOF — the loop
    // never reached the insertion points.
    if head_start >= lines.len() {
        out.push_str(&inject_head);
    }
    if head_end >= lines.len() {
        if let Some(g) = &guard_line {
            out.push_str(g);
        }
    }
    out
}

/// Names of projects bound to `(client, queue)` — powers the
/// submit-time hint when a projectless submit lands on a bound pair
/// (issue #14). Best-effort: a project file that fails to load is
/// skipped and registry errors yield no hint rather than breaking a
/// submission.
pub fn bound_projects(client_name: &str, queue: &str) -> Vec<String> {
    let Ok(names) = registry::list_names("projects") else {
        return Vec::new();
    };
    names.into_iter().filter_map(|n| registry::load_project(&n).ok()).filter(|p| p.bind.iter().any(|b| b.client == client_name && b.queue == queue)).map(|p| p.name).collect()
}

/// New server-side task id: `<unix>-<pidseq>`, deduped against the
/// spool dir. `pidseq` packs pid<<16 | per-process seq so two
/// same-second submits still differ in the 8-char tail `submit`
/// embeds in the pushed slug — §4.4 wants a re-pushed task
/// recognizable, which identical slugs would defeat.
fn new_task_id(dir: &std::path::Path) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let tag = ((std::process::id() as u64) << 16) | (SEQ.fetch_add(1, Ordering::Relaxed) & 0xffff);
    let base = format!("{:x}-{:x}", unix_now(), tag);
    for i in 0..1000 {
        let id = if i == 0 { base.clone() } else { format!("{base}-{i}") };
        if !dir.join(format!("{id}.json")).exists() {
            return id;
        }
    }
    base
}

/// `shunt submit`: resolve client (+ project binding), build the task
/// body, stage it. Returns the task id.
pub fn submit(client_name: &str, project: Option<&str>, guard: Option<&str>, slug: Option<&str>, file: Option<&str>, flags: &TaskFlags, cmd: &[String]) -> Result<String> {
    submit_to(client_name, None, project, guard, slug, file, flags, None, None, cmd)
}

#[allow(clippy::too_many_arguments)]
pub fn submit_to(client_name: &str, queue: Option<&str>, project: Option<&str>, guard: Option<&str>, slug: Option<&str>, file: Option<&str>, flags: &TaskFlags, hold: Option<String>, hold_on_box: Option<String>, cmd: &[String]) -> Result<String> {
    let client = registry::load_client(client_name)?;
    // `--project` resolves through the name middleware like `-c` —
    // a case-folded hit substitutes the canonical spelling with a
    // note; a miss names did-you-mean candidates (§4.1).
    let project = match project {
        Some(p) => Some(registry::names::into_resolved(registry::names::resolve_project(p)?, "project")?),
        None => None,
    };
    let project_entry = project.as_deref().map(registry::load_project).transpose()?;
    // A sole binding is unambiguous even when the client's default differs.
    let binding_queue = project_entry.as_ref().and_then(|p| {
        let mut bindings = p.bind.iter().filter(|b| b.client == client_name);
        let first = bindings.next()?;
        if bindings.next().is_none() {
            Some(first.queue.as_str())
        } else {
            None
        }
    });
    let target = registry::resolve_queue(&client, queue.or(binding_queue))?;

    // Project binding supplies workdir + env for the record; the body
    // itself carries no `cd` (the runner places cwd, design.md §3.12).
    let mut meta = TaskMeta { id: None, slug: String::new(), guard: guard.map(str::to_string), workdir: None, env: Default::default(), cmd: Vec::new(), timeout_s: flags.timeout_s.filter(|&n| n > 0), retry: flags.retry.filter(|&n| n > 0), retry_delay_s: flags.retry_delay_s, gpu_mem_mb: flags.gpu_mem_mb, ram_mb: flags.ram_mb, cpu_cores: flags.cpu_cores };
    if let Some(p) = &project_entry {
        let b = p.bind.iter().find(|b| b.client == client_name && b.queue == target.name).with_context(|| format!("project {} is not bound to client {client_name} queue {}; run `shunt project bind {} {client_name} --queue {} --workdir <path>`", p.name, target.name, p.name, target.name))?;
        meta.workdir = Some(b.workdir.clone());
        // project [defaults].env lays the floor; the binding's env
        // wins per key.
        meta.env = p.defaults.env.clone();
        meta.env.extend(b.env.clone());
    }
    // CLI `--env` is the most specific layer: it beats the binding
    // (and therefore [defaults]) per key, which is how an agent pins
    // e.g. CUDA_VISIBLE_DEVICES onto one submission.
    meta.env.extend(flags.env.clone());

    // A projectless submit to a bound (client, queue) pair stages a
    // task with no workdir/env at all — the trap behind issue #14:
    // the staged `--file` body then looks like it dropped the
    // binding's `# shunt:workdir=` when the real cause is a missing
    // `-p`. Name the bound projects; the note never gates.
    if project_entry.is_none() {
        let bound = bound_projects(client_name, &target.name);
        if !bound.is_empty() {
            eprintln!("note: queue '{}' on '{client_name}' is bound by project(s) {}; submit with `-p <name>` to apply its workdir/env (this task carries none)", target.name, bound.join(", "));
        }
    }

    let body = if let Some(path) = file {
        // The positional file slot means a bare word lands here; the
        // hint separates a missing script from a meant-as-command.
        fs::read_to_string(path).with_context(|| format!("cannot read task file {path:?} (to run a command, put it after `--`)"))?
    } else {
        if cmd.is_empty() {
            bail!("submit needs a command after `--` or --file <task.sh>");
        }
        meta.cmd = cmd.to_vec();
        String::new() // rendered after slug is known
    };

    let raw_slug = slug.map(str::to_string).unwrap_or_else(|| if let Some(path) = file { std::path::Path::new(path).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "task".into()) } else { std::path::Path::new(&cmd[0]).file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "task".into()) });
    let slug = sanitize_slug(&raw_slug);
    if let Some(p) = crate::proto::task::slug_problem(&slug) {
        bail!("bad slug '{raw_slug}': {p}");
    }

    let dir = spool_dir()?;
    crate::server::fsutil::private_dir(&dir)?;
    let id = new_task_id(&dir);
    // Tail, not head: the unix hex is >=8 chars until 2106, so
    // take(8) would capture only the timestamp — the discriminating
    // pidseq lives at the end of the id.
    let shortid: String = id.chars().rev().take(8).collect::<Vec<_>>().into_iter().rev().collect();
    meta.slug = format!("{slug}-{shortid}");
    // The id rides in the rendered body (`# shunt:id=`) so the
    // box-side allocator can return the existing entry on a re-push
    // after a lost ack instead of minting a second number.
    meta.id = Some(id.clone());

    crate::proto::task::validate_meta(&meta)?;
    let body = if file.is_some() { merge_file_directives(&body, &meta) } else { crate::client::exec::render_task(&meta) };

    // Guard contract (design.md §3.13): requeue reruns the body, so a
    // task with no guard line — neither `--guard` nor an embedded
    // check — double-runs on a lost ack or an orphan requeue. Warn
    // loudly; naturally idempotent tasks are legitimate but rare.
    if !body.contains("guard hit:") {
        eprintln!("submit: WARNING: '{}' carries no guard; at-least-once requeue will run it twice (lost ack, orphan requeue). Pass --guard <path> unless the task is naturally idempotent.", meta.slug);
    }

    let entry = SpoolEntry { id: id.clone(), project: project.unwrap_or_default(), client: client.name.clone(), queue: target.name, qroot: Some(target.client.qroot), hold, hold_on_box, meta, submitted_at: rfc3339_z_now() };
    stage(&entry, &body)?;
    Ok(id)
}

/// Persist a spool entry + rendered task body, each file via
/// write-temp-then-rename. The `.sh` body commits FIRST and the
/// `.json` marker LAST: `list()` keys on `.json`, so a crash
/// mid-stage must never leave a parseable `.json` whose body is
/// missing (dispatch would retain and re-log it every sweep
/// forever). A crash here instead orphans only the `.sh`, which
/// `list()` never sees.
pub fn stage(entry: &SpoolEntry, body: &str) -> Result<()> {
    let dir = spool_dir()?;
    crate::server::fsutil::private_dir(&dir)?;
    let json = serde_json::to_string_pretty(entry)?;
    for (name, contents) in [(format!("{}.sh", entry.id), body.to_string()), (format!("{}.json", entry.id), json)] {
        crate::server::fsutil::atomic_write(&dir.join(name), contents)?;
    }
    Ok(())
}

/// Pin legacy targets before any delivery attempt, including one whose ack is lost.
pub(crate) fn pin_target(entry: &mut SpoolEntry, qroot: &str) -> Result<()> {
    validate_qroot(entry.qroot.as_deref(), qroot)?;
    if entry.qroot.is_none() {
        let mut pinned = entry.clone();
        pinned.qroot = Some(qroot.into());
        let dir = spool_dir()?;
        valid_task_id(&entry.id)?;
        crate::server::fsutil::atomic_write(&dir.join(format!("{}.json", entry.id)), serde_json::to_string_pretty(&pinned)?)?;
        *entry = pinned;
    }
    Ok(())
}

pub(crate) fn validate_qroot(recorded: Option<&str>, current: &str) -> Result<()> {
    if let Some(recorded) = recorded {
        anyhow::ensure!(recorded == current, "queue qroot changed from {recorded:?} to {current:?}; restore the recorded queue path before retrying; shunt does not relocate historical tasks");
    }
    Ok(())
}

/// Oldest-first listing of staged entries (submitted_at sorts
/// lexically; id breaks ties). Per-entry fault tolerance: an entry
/// that fails to read or parse is quarantined — the `.json` (and its
/// `.sh` sibling) renamed to `*.bad` — with a stderr warning, so one
/// corrupt file can never wedge the whole sweep (design.md §4.4).
pub fn list() -> Result<Vec<SpoolEntry>> {
    let dir = spool_dir()?;
    let mut out = Vec::new();
    match fs::read_dir(&dir) {
        Ok(rd) => {
            for ent in rd {
                let ent = match ent {
                    Ok(e) => e,
                    Err(e) => {
                        eprintln!("spool: cannot stat entry in {}: {e}", dir.display());
                        continue;
                    }
                };
                let name = ent.file_name().to_string_lossy().into_owned();
                let Some(id) = name.strip_suffix(".json") else { continue };
                if id.starts_with('.') {
                    continue;
                }
                match read_entry(&dir, &name) {
                    Ok(e) => out.push(e),
                    Err(e) => {
                        eprintln!("spool: {e:#}; quarantining {id}.* as .bad");
                        quarantine(&dir, id);
                    }
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", dir.display())),
    }
    out.sort_by(|a, b| a.submitted_at.cmp(&b.submitted_at).then(a.id.cmp(&b.id)));
    Ok(out)
}

/// Task ids are filenames under spool/ and tasks/ — reject
/// anything that is not `[A-Za-z0-9_-]+`: a `../`-carrying id
/// (CLI-supplied or planted in a parsed entry) would escape the
/// directory it is joined into.
pub(crate) fn valid_task_id(id: &str) -> Result<()> {
    anyhow::ensure!(!id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-')), "bad task id {id:?}");
    Ok(())
}

fn read_entry(dir: &std::path::Path, name: &str) -> Result<SpoolEntry> {
    let text = fs::read_to_string(dir.join(name)).with_context(|| format!("cannot read spool entry {name}"))?;
    let e: SpoolEntry = serde_json::from_str(&text).with_context(|| format!("bad JSON in spool/{name}"))?;
    // The parsed id feeds body()/remove() paths — it must be a safe
    // filename and equal to the entry's own stem, or a planted
    // `"id":"../../x"` would read/delete outside spool/.
    let stem = name.strip_suffix(".json").unwrap_or(name);
    anyhow::ensure!(valid_task_id(&e.id).is_ok() && e.id == stem, "spool/{name}: id {:?} is unsafe or does not match its filename", e.id);
    Ok(e)
}

/// Rename a bad entry's files to `*.bad` so listings skip them but
/// they stay on disk for inspection. Best-effort: a failed rename
/// warns and the entry is still skipped this round.
fn quarantine(dir: &std::path::Path, id: &str) {
    for ext in ["json", "sh"] {
        let from = dir.join(format!("{id}.{ext}"));
        match fs::rename(&from, dir.join(format!("{id}.{ext}.bad"))) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => eprintln!("spool: cannot quarantine {}: {e}", from.display()),
        }
    }
}

/// Read the staged task body for an entry.
pub fn body(task_id: &str) -> Result<String> {
    valid_task_id(task_id)?;
    let path = spool_dir()?.join(format!("{task_id}.sh"));
    fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))
}

/// Resolve a task-id argument to a staged id: exact match first,
/// else a unique prefix (same affordance as `task show`).
fn resolve_id(id_or_prefix: &str) -> Result<String> {
    valid_task_id(id_or_prefix)?;
    if spool_dir()?.join(format!("{id_or_prefix}.json")).exists() {
        return Ok(id_or_prefix.to_string());
    }
    let matches: Vec<String> = list()?.into_iter().filter(|e| e.id.starts_with(id_or_prefix)).map(|e| e.id).collect();
    match matches.len() {
        1 => Ok(matches.into_iter().next().unwrap()),
        0 => bail!("no spool entry with id {id_or_prefix:?}"),
        _ => bail!("id prefix {id_or_prefix:?} matches several spool entries"),
    }
}

/// Set or clear an entry's hold flag (design.md §4.4): `Some`
/// holds it out of every dispatch sweep until released, `None`
/// releases it. Returns the resolved task id.
pub fn set_hold(id_or_prefix: &str, hold: Option<String>) -> Result<String> {
    let id = resolve_id(id_or_prefix)?;
    let dir = spool_dir()?;
    let mut entry = read_entry(&dir, &format!("{id}.json"))?;
    entry.hold = hold;
    crate::server::fsutil::atomic_write(&dir.join(format!("{id}.json")), serde_json::to_string_pretty(&entry)?)?;
    Ok(id)
}

/// Remove an entry after a clean enqueue ack (both files).
pub fn remove(task_id: &str) -> Result<()> {
    valid_task_id(task_id)?;
    let dir = spool_dir()?;
    for ext in ["json", "sh"] {
        match fs::remove_file(dir.join(format!("{task_id}.{ext}"))) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e).with_context(|| format!("cannot remove spool/{task_id}.{ext}"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_matches_task_format() {
        let meta = TaskMeta { id: Some("deadbeef-01".into()), slug: "train-v2-ab12".into(), guard: Some("runs/x.json".into()), workdir: Some("/root/autodl-tmp/P".into()), env: Default::default(), cmd: vec!["uv".into(), "run".into(), "python".into(), "a b.py".into()], timeout_s: None, retry: None, retry_delay_s: None, gpu_mem_mb: None, ram_mb: None, cpu_cores: None };
        let body = crate::client::exec::render_task(&meta);
        assert!(body.starts_with("#!/usr/bin/env bash\n"));
        assert!(body.contains("# task: train-v2-ab12 (generated by shunt "));
        assert!(body.contains("# shunt:id=deadbeef-01\n"));
        assert!(body.contains("# shunt:workdir=/root/autodl-tmp/P\n"));
        assert!(body.contains("set -uo pipefail\n"));
        assert!(body.contains("if [ -e runs/x.json ]; then"));
        assert!(body.contains("exec uv run python 'a b.py'\n"));
        assert!(!body.contains("cd "));
    }

    /// Same injection surface as render_task: a `--guard` merged into
    /// a `--file` body must appear in the echo line only shell-quoted.
    #[test]
    fn file_merge_guard_echo_uses_quoted_form() {
        for guard in ["runs/$(touch X).json", "runs/evil\".json", "runs/`id`.json"] {
            let meta = TaskMeta { id: None, slug: "t".into(), guard: Some(guard.into()), workdir: None, env: Default::default(), cmd: Vec::new(), timeout_s: None, retry: None, retry_delay_s: None, gpu_mem_mb: None, ram_mb: None, cpu_cores: None };
            let out = merge_file_directives("#!/usr/bin/env bash\necho hi\n", &meta);
            let q = shell_quote(guard);
            let line = format!("if [ -e {q} ]; then printf 'guard hit: %s exists; skipping\\n' {q}; exit 0; fi");
            assert!(out.contains(&line), "merged body missing quoted guard line for {guard:?}: {out}");
            // The quoted guard must never sit inside a double-quoted
            // string, where its `$()`/backticks would re-expand.
            assert!(!out.contains(&format!("\"guard hit: {q}")), "quoted guard inside a double-quoted echo for {guard:?}");
        }
    }

    /// Two consecutive ids minted in the same process (i.e. possibly
    /// the same second) must differ in the tail `submit` uses as
    /// shortid, and both must fit the valid_task_id charset.
    #[test]
    fn same_second_ids_differ_in_shortid_tail() {
        let dir = std::env::temp_dir().join(format!("shunt-spool-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let (a, b) = (new_task_id(&dir), new_task_id(&dir));
        assert_ne!(a, b);
        let tail = |s: &str| s.chars().rev().take(8).collect::<String>();
        assert_ne!(tail(&a), tail(&b), "same-second submits would share a pushed slug: {a} vs {b}");
        for id in [a, b] {
            valid_task_id(&id).unwrap();
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
