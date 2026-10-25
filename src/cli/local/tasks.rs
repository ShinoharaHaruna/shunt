//! Task ops: enqueue/new/cancel/prio/kill — each with a wire twin.

use anyhow::{Context as _, Result};

use super::{jfail, jfields, jobj, jprint, local_root, wire};
use crate::cli::args::{CatArgs, EnqueueArgs, NewArgs, PatternArgs, PrioArgs};

pub(crate) fn agent_enqueue(a: &EnqueueArgs) -> Result<()> {
    wire(a.json, enqueue_impl(a))
}

fn enqueue_impl(a: &EnqueueArgs) -> Result<()> {
    use std::io::Read;
    let root = local_root(&a.root)?;
    let inner = || -> Result<crate::client::queue::EnqueueOutcome> {
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
        crate::client::queue::enqueue(&root, &slug, &body)
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
                if crate::client::daemon::alive(&root)?.is_none() {
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
    let inner = || -> Result<crate::client::queue::EnqueueOutcome> {
        let meta = a.task_meta()?;
        let body = crate::client::exec::render_task(&meta);
        let root = local_root(&a.root)?;
        crate::client::queue::enqueue(&root, &meta.slug, &body)
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
                let root = local_root(&a.root)?;
                if crate::client::daemon::alive(&root)?.is_none() {
                    eprintln!("note: queue daemon not running; start with `shunt start`");
                }
            }
            Ok(())
        }
        Err(e) => jfail(json, &format!("{e:#}")),
    }
}

/// The 0-match refusal for a pattern op scoped to `track`. A miss
/// can still be a live task one track over — the daemon's claim
/// moves pending → running mid-typing, after which every spelling
/// of the name reads "no pending task matches" (issue #15). Name
/// the pattern's hits in the OTHER tracks so the refusal steers
/// to the command that reaches them (`pending` → `cancel`,
/// `running` → `kill`); a hit nowhere stays a plain miss. For
/// `kill`, whose own list is pid-gated (§3.9), a claimed but
/// pid-less running entry is named too — it sits in the searched
/// track yet cannot be signaled for a moment.
fn miss_msg(root: &std::path::Path, track: &str, pat: &str) -> String {
    let names = |t: &str| -> Vec<String> { crate::client::queue::list_entries(root, t).unwrap_or_default().into_iter().filter(|n| n.contains(pat)).collect() };
    let mut hints = Vec::new();
    if track == "running" {
        let starting: Vec<String> = names("running").into_iter().filter(|n| !crate::client::queue::piece_file(root, "running", n, "pid").exists()).collect();
        if !starting.is_empty() {
            hints.push(format!("claimed, no pid yet: {}", starting.join(", ")));
        }
    }
    for t in ["pending", "running", "done", "failed"] {
        if t == track {
            continue;
        }
        let hits = names(t);
        if hits.is_empty() {
            continue;
        }
        let steer = match t {
            "pending" => " (`shunt cancel` reaches those)",
            "running" => " (`shunt kill` reaches those)",
            _ => "",
        };
        hints.push(format!("{t}: {}{steer}", hits.join(", ")));
    }
    let mut msg = format!("no {track} task matches '{pat}'");
    if !hints.is_empty() {
        msg.push_str(&format!(" — pattern does match {}", hints.join("; ")));
    }
    msg
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
    let root = local_root(&a.root)?;
    let m = crate::client::queue::match_pending(&root, &a.pattern)?;
    let name = one_match(&m, a.json, &miss_msg(&root, "pending", &a.pattern), &format!("pattern matches several tasks, refusing:\n  {}", m.join("\n  ")))?;
    crate::client::queue::remove_pending(&root, &name)?;
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
    let root = local_root(&a.root)?;
    if a.n > crate::client::queue::PRIORITY_BAND_MAX {
        return jfail(a.json, &format!("prio number must be inside the 0000-{:04} priority band, got {:04}", crate::client::queue::PRIORITY_BAND_MAX, a.n));
    }
    let m = crate::client::queue::match_pending(&root, &a.pattern)?;
    let miss = if m.is_empty() { miss_msg(&root, "pending", &a.pattern) } else { format!("need exactly one pending match, got {}:\n  {}", m.len(), m.join("\n  ")) };
    let base = one_match(&m, a.json, &miss, &miss)?;
    let rest = base.split_once('_').map(|(_, r)| r).unwrap_or(base.as_str());
    let new = format!("{:04}_{rest}", a.n);
    // Entry shapes: a dir renames to a dir; a legacy flat file keeps
    // its `.sh` suffix so it stays visible to the listing.
    let (from, to) = if crate::client::queue::is_dir_entry(&root, "pending", &base) { (root.join("pending").join(&base), root.join("pending").join(&new)) } else { (root.join("pending").join(format!("{base}.sh")), root.join("pending").join(format!("{new}.sh"))) };
    // refuse to clobber an existing pending task (bash `mv -f` defect,
    // design.md §3.14); check BOTH shapes for BOTH entry shapes — a
    // flat source renaming onto a same-named pending DIR would leave
    // `NNNN_x/` + `NNNN_x.sh` coexisting as two tasks.
    let occupied = root.join("pending").join(&new).exists() || root.join("pending").join(format!("{new}.sh")).exists();
    if occupied && new != base {
        return jfail(a.json, &format!("target name exists: {new}"));
    }
    if new != base {
        if crate::client::queue::is_dir_entry(&root, "pending", &base) {
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

/// Entries matching `pat` across the four live tracks, in state-
/// machine order. An exact name match wins outright (an entry lives
/// in exactly one track, so it is inherently singular) — otherwise
/// every substring hit is a candidate the caller reduces.
pub(crate) fn find_task(root: &std::path::Path, pat: &str) -> Vec<(&'static str, String)> {
    let mut subs = Vec::new();
    for t in ["pending", "running", "done", "failed"] {
        for n in crate::client::queue::list_entries(root, t).unwrap_or_default() {
            if n == pat {
                return vec![(t, n)];
            }
            if n.contains(pat) {
                subs.push((t, n));
            }
        }
    }
    subs
}

/// `cat` prints a task's task.sh — the pending track's only
/// artifact (no log exists until a claim), and the box-side half
/// of the dashboard's pending-entry script pane. Text op like
/// `logs`/`mktask`: the body goes to stdout verbatim, not JSON.
pub(crate) fn agent_cat(a: &CatArgs) -> Result<()> {
    let root = local_root(&a.root)?;
    let m = find_task(&root, &a.pattern);
    match m.as_slice() {
        [(track, name)] => {
            let tf = crate::client::queue::task_file(&root, track, name);
            let bytes = std::fs::read(&tf).with_context(|| format!("cannot read {}", tf.display()))?;
            use std::io::Write;
            std::io::stdout().write_all(&bytes)?;
            Ok(())
        }
        [] => anyhow::bail!("no task matches '{}'", a.pattern),
        _ => {
            let list = m.iter().map(|(t, n)| format!("{t}: {n}")).collect::<Vec<_>>().join("\n  ");
            anyhow::bail!("pattern matches several tasks, refusing:\n  {list}")
        }
    }
}

pub(crate) fn agent_kill(a: &PatternArgs) -> Result<()> {
    wire(a.json, kill_impl(a))
}

fn kill_impl(a: &PatternArgs) -> Result<()> {
    let root = local_root(&a.root)?;
    let m = crate::client::queue::match_running(&root, &a.pattern)?;
    let name = one_match(&m, a.json, &miss_msg(&root, "running", &a.pattern), &format!("pattern matches several tasks, refusing:\n  {}", m.join("\n  ")))?;
    match crate::client::exec::kill_task(&root, &name) {
        Ok(crate::client::exec::KillOutcome::Gone) => {
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
        Ok(crate::client::exec::KillOutcome::Signaled { pgid, escalated }) => {
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
