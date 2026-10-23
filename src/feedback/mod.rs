//! Feedback channel (design.md §14): file a report as a GitHub
//! issue. The reporter is usually an agent in another project, so
//! the pipeline is persist-first (at-least-once, like the spool) and
//! sanitize-always (the default repo is public).
//!
//! Deliberately not a wire op and not box-aware: whatever machine
//! runs it files directly; a box without gh/token just keeps the
//! artifact and the error names where it landed.

mod compose;
mod deliver;
mod ledger;
mod sanitize;

use anyhow::Result;
use std::io::Write;
use std::path::PathBuf;

/// Compose + sanitize + persist + dedup + rate-cap + deliver.
/// Prints the human or `--json` result; delivery failure leaves a
/// pending artifact and returns Err (with a JSON error line first
/// when `json` is set — agents consume this command too).
pub fn send(kind: &str, title: &str, body: &str, client: Option<&str>, dry_run: bool, json: bool) -> Result<()> {
    let body = compose::body(body, client)?;

    // Sanitize before anything is persisted — this is the boundary:
    // the ledger artifact and the delivered payload are both the
    // scrubbed text, so a later `retry` can never deliver a
    // pre-scrub draft. Two passes over title and body: secret
    // values (exact match) then registered names (case-insensitive,
    // kind-tagged placeholders). The composed body carries the
    // context block, so `--client` facts are covered too. The
    // `[feedback/<kind>]` prefix is added only after scrubbing —
    // kind is a fixed enum, nothing user-controlled enters
    // post-scrub, and a client literally named "bug" cannot eat it.
    let secrets = sanitize::secrets();
    let names = sanitize::names();
    let (title, n1) = sanitize::scrub(title, &secrets);
    let (title, n2) = sanitize::scrub_names(&title, &names);
    let issue_title = format!("[feedback/{kind}] {title}");
    let (body, n3) = sanitize::scrub(&body, &secrets);
    let (body, n4) = sanitize::scrub_names(&body, &names);
    let n = n1 + n2 + n3 + n4;
    if n > 0 {
        eprintln!("redacted {n} sensitive value(s)");
    }

    if dry_run {
        println!("{issue_title}\n\n{body}");
        return Ok(());
    }

    let mut art = ledger::Artifact { kind: kind.into(), title, body, created_at: now(), delivered_at: None, issue_url: None, issue_number: None, attempts: 0 };
    // Persist first: a report is never lost to a delivery failure.
    // Warn-only — delivery may still succeed, and if it also fails
    // the composed body is dumped to stderr as the last resort.
    let path = match ledger::save_new(&art) {
        Ok(p) => Some(p),
        Err(e) => {
            eprintln!("warning: cannot persist feedback artifact ({e:#}) — continuing to delivery");
            None
        }
    };

    match deliver_now(&mut art, &issue_title) {
        Ok(deduped) => {
            if let Some(p) = &path {
                if let Err(e) = ledger::save_at(p, &art) {
                    eprintln!("warning: delivered but cannot update {} ({e:#})", p.display());
                }
            }
            report(json, &art, deduped);
            Ok(())
        }
        Err(e) => fail(json, e, path.as_ref(), &issue_title, &art.body),
    }
}

/// Dedup + rate cap + the actual delivery attempt. Mutates the
/// artifact's delivery fields. Ok(deduped); Err = still pending.
fn deliver_now(art: &mut ledger::Artifact, issue_title: &str) -> Result<bool> {
    // Dedup runs before the cap: a repeat report answers with the
    // existing URL and must never be refused.
    if let Some(prev) = ledger::find_dup(art, 86400)? {
        if let (Some(u), Some(n), t) = (prev.issue_url, prev.issue_number, prev.delivered_at) {
            art.delivered_at = t;
            art.issue_url = Some(u);
            art.issue_number = Some(n);
            return Ok(true);
        }
    }
    let cap = max_per_hour();
    let sent = ledger::delivered_count(3600)?;
    anyhow::ensure!(sent < cap, "feedback rate cap reached ({sent}/{cap} delivered in the last hour) — artifact persisted; re-run `shunt feedback retry` later");

    let repo = deliver::resolve_repo()?;
    art.attempts += 1;
    let d = deliver::deliver(&repo, issue_title, &art.body)?;
    art.delivered_at = Some(now());
    art.issue_url = Some(d.url);
    art.issue_number = Some(d.number);
    Ok(false)
}

/// Emit the failure honestly, then Err. When nothing was persisted
/// the composed report goes to stderr so it survives in scrollback.
fn fail(json: bool, err: anyhow::Error, path: Option<&PathBuf>, issue_title: &str, body: &str) -> Result<()> {
    if json {
        let mut o = serde_json::json!({"ok": false, "error": format!("{err:#}")});
        if let Some(p) = path {
            o["artifact"] = serde_json::json!(p.display().to_string());
        }
        println!("{}", serde_json::to_string(&o).unwrap_or_default());
    }
    let hint = match path {
        Some(p) => format!("; report persisted at {} — re-run `shunt feedback retry`", p.display()),
        None => {
            let _ = writeln!(std::io::stderr(), "=== undelivered feedback report ===\n{issue_title}\n\n{body}");
            "; report could not be persisted either — full text dumped to stderr".to_string()
        }
    };
    Err(err.context(format!("feedback not delivered{hint}")))
}

fn report(json: bool, art: &ledger::Artifact, deduped: bool) {
    if json {
        println!("{}", serde_json::json!({"ok": true, "url": art.issue_url, "number": art.issue_number, "deduped": deduped}));
    } else {
        let verb = if deduped { "already filed" } else { "filed" };
        println!("feedback {verb}: {}", art.issue_url.as_deref().unwrap_or("?"));
    }
}

/// `feedback ls`: one row or one JSON object per artifact — header
/// fields only, so a long ledger does not load every report body.
pub fn ls(json: bool) -> Result<()> {
    for (path, art) in ledger::list_meta()? {
        let id = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let state = match (&art.issue_url, art.delivered_at) {
            (Some(u), Some(_)) => format!("filed #{}", u.rsplit('/').next().unwrap_or("?")),
            _ => format!("pending ({} attempt(s))", art.attempts),
        };
        if json {
            println!("{}", serde_json::json!({"id": id, "kind": art.kind, "title": art.title, "delivered": art.delivered_at.is_some(), "url": art.issue_url, "attempts": art.attempts}));
        } else {
            println!("{}  {}  {:<22} {}", crate::proto::time::rfc3339_z(art.created_at), art.kind, state, art.title);
        }
    }
    Ok(())
}

/// `feedback retry [id]`: resolve an exact stem or unique ledger-wide
/// prefix before any delivery side effects; None = all pending.
/// Batch failures are reported per-artifact and surface after all attempts.
pub fn retry(id: Option<&str>, json: bool) -> Result<()> {
    let mut matched = ledger::list()?;
    if let Some(id) = id {
        let stem = |p: &PathBuf| p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        // Resolve against delivered entries too: an id must not silently
        // change meaning when another matching artifact gets filed.
        if let Some(exact) = matched.iter().position(|(p, _)| stem(p) == id) {
            matched = vec![matched.swap_remove(exact)];
        } else {
            matched.retain(|(p, _)| stem(p).starts_with(id));
        }
        anyhow::ensure!(!matched.is_empty(), "no feedback artifact matching {id:?} — run `shunt feedback ls --json` to find an id");
        anyhow::ensure!(matched.len() == 1, "ambiguous feedback id {id:?}; candidates: {} — re-run `shunt feedback retry <full-id>` with one listed filename stem", matched.iter().map(|(p, _)| stem(p)).collect::<Vec<_>>().join(", "));
        let (path, art) = &matched[0];
        anyhow::ensure!(art.delivered_at.is_none(), "feedback {} already filed: {} (filed artifacts are not retried) — run `shunt feedback ls --json` to find a pending id", stem(path), art.issue_url.as_deref().unwrap_or("URL unavailable"));
    } else {
        matched.retain(|(_, a)| a.delivered_at.is_none());
    }
    anyhow::ensure!(!matched.is_empty(), "no pending feedback artifacts");

    let mut failed = 0usize;
    for (path, mut art) in matched {
        let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        // Re-scrub before re-delivery: the artifact was sanitized at
        // send time, but a client row registered since can have added
        // secrets or names the persisted body still carries.
        let secrets = sanitize::secrets();
        let names = sanitize::names();
        let (b, n1) = sanitize::scrub(&art.body, &secrets);
        let (b, n2) = sanitize::scrub_names(&b, &names);
        let (t, n3) = sanitize::scrub(&art.title, &secrets);
        let (t, n4) = sanitize::scrub_names(&t, &names);
        if n1 + n2 + n3 + n4 > 0 {
            eprintln!("{stem}: redacted {} new sensitive value(s)", n1 + n2 + n3 + n4);
            art.body = b;
            art.title = t;
        }
        let issue_title = format!("[feedback/{}] {}", art.kind, art.title);
        // Same resolve path as send: dedup and the rate cap still
        // apply — the cap's own remediation is "retry later", so a
        // bypass here would let a looped caller spam a public repo.
        match deliver_now(&mut art, &issue_title) {
            Ok(deduped) => {
                if let Err(e) = ledger::save_at(&path, &art) {
                    eprintln!("warning: delivered but cannot update {} ({e:#})", path.display());
                }
                if json {
                    println!("{}", serde_json::json!({"ok": true, "id": stem, "url": art.issue_url, "number": art.issue_number, "deduped": deduped}));
                } else {
                    let verb = if deduped { "already filed" } else { "filed" };
                    println!("{stem}: {verb} {}", art.issue_url.as_deref().unwrap_or("?"));
                }
            }
            Err(e) => {
                failed += 1;
                let _ = ledger::save_at(&path, &art); // keep the attempts count honest
                if json {
                    println!("{}", serde_json::json!({"ok": false, "id": stem, "error": format!("{e:#}")}));
                } else {
                    eprintln!("{stem}: {e:#}");
                }
            }
        }
    }
    anyhow::ensure!(failed == 0, "{failed} artifact(s) still undelivered");
    Ok(())
}

fn max_per_hour() -> u64 {
    match std::env::var("SHUNT_FEEDBACK_MAX_PER_HOUR") {
        Ok(v) => match v.parse() {
            Ok(n) => n,
            // A garbage cap must not silently become 5 — the reader
            // wrote it expecting it to apply.
            Err(_) => {
                eprintln!("warning: SHUNT_FEEDBACK_MAX_PER_HOUR={v:?} is not a number; using default 5");
                5
            }
        },
        Err(_) => 5,
    }
}

pub(crate) fn now() -> u64 {
    crate::proto::time::unix_now()
}
