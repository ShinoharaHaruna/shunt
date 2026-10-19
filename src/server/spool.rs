//! Server-side staging queue (design.md §4.4). Entries persist under
//! `<SHUNT_HOME>/spool/` as `<task-id>.json` + `<task-id>.sh` so a
//! submission survives while the box is powered off.

use crate::proto::task::{sanitize_slug, SpoolEntry, TaskMeta};
use crate::server::registry::{self, shunt_home};
use crate::server::transport::shell_quote;
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

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
    let mut have_env: std::collections::BTreeSet<String> = Default::default();
    for l in &lines[head_start..head_end] {
        if l.starts_with("# shunt:workdir=") {
            have_workdir = true;
        } else if l.starts_with("# shunt:timeout_s=") {
            have_timeout = true;
        } else if let Some(kv) = l.strip_prefix("# shunt-env:") {
            if let Some((k, _)) = kv.split_once('=') {
                have_env.insert(k.trim().to_string());
            }
        }
    }

    let mut inject_head = String::new();
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
    for (k, v) in &meta.env {
        if !have_env.contains(k) {
            inject_head.push_str(&format!("# shunt-env:{k}={v}\n"));
        }
    }
    let guard_line = meta.guard.as_ref().map(|g| {
        let q = shell_quote(g);
        format!("if [ -e {q} ]; then echo \"guard hit: {q} exists; skipping\"; exit 0; fi\n")
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

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

/// Unix seconds → `YYYY-MM-DDTHH:MM:SSZ` (UTC, civil-from-days).
pub fn iso_utc(secs: u64) -> String {
    let days = secs / 86400;
    let rem = secs % 86400;
    let (h, m, s) = (rem / 3600, rem % 3600 / 60, rem % 60);
    // Howard Hinnant's civil_from_days.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// New server-side task id: `<unix>-<pid>`, deduped against spool dir.
fn new_task_id(dir: &std::path::Path) -> String {
    let base = format!("{:x}-{:x}", now_unix(), std::process::id());
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
pub fn submit(client_name: &str, project: Option<&str>, guard: Option<&str>, slug: Option<&str>, file: Option<&str>, timeout_s: Option<u64>, cmd: &[String]) -> Result<String> {
    let client = registry::load_client(client_name)?;

    // Project binding supplies workdir + env for the record; the body
    // itself carries no `cd` (the runner places cwd, design.md §3.12).
    let mut meta = TaskMeta { slug: String::new(), guard: guard.map(str::to_string), workdir: None, env: Default::default(), cmd: Vec::new(), timeout_s: timeout_s.filter(|&n| n > 0) };
    if let Some(pname) = project {
        let p = registry::load_project(pname)?;
        let b = p.bind.iter().find(|b| b.client == client_name).with_context(|| format!("project {pname} is not bound to client {client_name}"))?;
        meta.workdir = Some(b.workdir.clone());
        // project [defaults].env lays the floor; the binding's env
        // wins per key.
        meta.env = p.defaults.env.clone();
        meta.env.extend(b.env.clone());
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
    let shortid: String = id.chars().take(8).collect();
    meta.slug = format!("{slug}-{shortid}");

    crate::proto::task::validate_meta(&meta)?;
    let body = if file.is_some() { merge_file_directives(&body, &meta) } else { crate::agent::exec::render_task(&meta) };

    // Guard contract (design.md §3.13): requeue reruns the body, so a
    // task with no guard line — neither `--guard` nor an embedded
    // check — double-runs on a lost ack or an orphan requeue. Warn
    // loudly; naturally idempotent tasks are legitimate but rare.
    if !body.contains("guard hit:") {
        eprintln!("submit: WARNING: '{}' carries no guard; at-least-once requeue will run it twice (lost ack, orphan requeue). Pass --guard <path> unless the task is naturally idempotent.", meta.slug);
    }

    let entry = SpoolEntry { id: id.clone(), project: project.unwrap_or("").to_string(), client: client.name.clone(), meta, submitted_at: iso_utc(now_unix()) };
    stage(&entry, &body)?;
    Ok(id)
}

/// Persist a spool entry + rendered task body, each file via
/// write-temp-then-rename.
pub fn stage(entry: &SpoolEntry, body: &str) -> Result<()> {
    let dir = spool_dir()?;
    crate::server::fsutil::private_dir(&dir)?;
    let json = serde_json::to_string_pretty(entry)?;
    for (name, contents) in [(format!("{}.json", entry.id), json), (format!("{}.sh", entry.id), body.to_string())] {
        let tmp = dir.join(format!(".{name}.tmp"));
        crate::server::fsutil::write_private(&tmp, contents)?;
        fs::rename(&tmp, dir.join(&name))?;
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
        let meta = TaskMeta { slug: "train-v2-ab12".into(), guard: Some("runs/x.json".into()), workdir: Some("/root/autodl-tmp/P".into()), env: Default::default(), cmd: vec!["uv".into(), "run".into(), "python".into(), "a b.py".into()], timeout_s: None };
        let body = crate::agent::exec::render_task(&meta);
        assert!(body.starts_with("#!/usr/bin/env bash\n"));
        assert!(body.contains("# task: train-v2-ab12 (generated by shunt "));
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
            let meta = TaskMeta { slug: "t".into(), guard: Some(guard.into()), workdir: None, env: Default::default(), cmd: Vec::new(), timeout_s: None };
            let out = merge_file_directives("#!/usr/bin/env bash\necho hi\n", &meta);
            let q = shell_quote(guard);
            let line = format!("if [ -e {q} ]; then echo \"guard hit: {q} exists; skipping\"; exit 0; fi");
            assert!(out.contains(&line), "merged body missing quoted guard line for {guard:?}: {out}");
            assert!(!out.contains(&format!("echo \"guard hit: {guard}")), "raw guard leaked into echo for {guard:?}");
        }
    }

    #[test]
    fn iso_epoch() {
        assert_eq!(iso_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_utc(1_700_000_000), "2023-11-14T22:13:20Z");
    }
}
