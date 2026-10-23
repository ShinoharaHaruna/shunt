//! Task ops: enqueue/new/cancel/prio/kill — each with a wire twin.

use anyhow::{Context as _, Result};

use super::{jfail, jfields, jobj, jprint, local_root, wire};
use crate::cli::args::{EnqueueArgs, NewArgs, PatternArgs, PrioArgs};

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
    let name = one_match(&m, a.json, &format!("no pending task matches '{}'", a.pattern), &format!("pattern matches several tasks, refusing:\n  {}", m.join("\n  ")))?;
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
    let miss = format!("need exactly one pending match, got {}:\n  {}", m.len(), m.join("\n  "));
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

pub(crate) fn agent_kill(a: &PatternArgs) -> Result<()> {
    wire(a.json, kill_impl(a))
}

fn kill_impl(a: &PatternArgs) -> Result<()> {
    let root = local_root(&a.root)?;
    let m = crate::client::queue::match_running(&root, &a.pattern)?;
    let name = one_match(&m, a.json, &format!("no running task matches '{}'", a.pattern), &format!("pattern matches several tasks, refusing:\n  {}", m.join("\n  ")))?;
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
