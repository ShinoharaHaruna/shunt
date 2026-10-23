//! `task show` read path (design.md §4.5).

use anyhow::{Context as _, Result};

/// outcome live from the box when none is recorded yet (lazy
/// write-back, design.md §4.5). A unique id prefix is accepted.
pub(crate) fn task_show(id: &str, json: bool) -> Result<()> {
    use crate::server::tasks::{self, Observed};
    let mut rec = match tasks::load(id) {
        Ok(r) => r,
        Err(e) => {
            // Prefix fallback: ids are long; a unique prefix resolves.
            let matches: Vec<_> = tasks::list()?.into_iter().filter(|r| r.id.starts_with(id)).collect();
            match matches.len() {
                1 => matches.into_iter().next().unwrap(),
                0 => return Err(e).context("no ledger record; the task may predate the ledger or the id is wrong"),
                _ => anyhow::bail!("id prefix '{id}' matches several tasks"),
            }
        }
    };
    let mut live: Option<String> = None;
    if rec.outcome.is_none() {
        match crate::server::transport::client_transport(&rec.client) {
            Ok((client, t)) => {
                live = Some(match tasks::observe(t.as_ref(), &client, &mut rec) {
                    Ok(obs) => match obs {
                        Observed::Pending => "pending".into(),
                        Observed::Running => "running".into(),
                        Observed::Done(rc) => format!("done rc={rc}"),
                        Observed::Failed(rc) => format!("failed rc={rc}"),
                        Observed::Unknown => "unknown".into(),
                        Observed::Unreachable(e) => format!("unreachable: {}", e.lines().next().unwrap_or(&e)),
                    },
                    Err(e) => format!("unreachable: {e:#}"),
                });
            }
            Err(e) => live = Some(format!("client unloadable: {e:#}")),
        }
    }
    if json {
        let mut v = serde_json::to_value(&rec)?;
        if let Some(l) = &live {
            v["live"] = serde_json::json!(l);
        }
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        println!("id:        {}", rec.id);
        println!("client:    {}", rec.client);
        println!("project:   {}", if rec.project.is_empty() { "-" } else { &rec.project });
        println!("slug:      {}", rec.slug);
        println!("queue:     {}  (on {}/{})", rec.queue_name, rec.client, rec.queue);
        println!("submitted: {}", rec.submitted_at);
        println!("pushed:    {}", rec.pushed_at);
        match (&rec.outcome, &live) {
            (Some(o), _) => println!("outcome:   {} rc={} (observed {})", o.state, o.rc, o.observed_at),
            (None, Some(l)) => println!("outcome:   {l} (live)"),
            (None, None) => println!("outcome:   not yet observed"),
        }
    }
    Ok(())
}
