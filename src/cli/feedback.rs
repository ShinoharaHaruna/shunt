//! `feedback` handler (design.md §14): body-source resolution lives
//! here (it's a CLI concern); compose/sanitize/ledger/deliver live
//! in crate::feedback.

use super::args::{FeedbackAction, FeedbackArgs};
use anyhow::{Context, Result};
use std::io::{IsTerminal, Read};

pub fn cmd_feedback(a: &FeedbackArgs) -> Result<()> {
    match &a.action {
        Some(FeedbackAction::Ls) => return crate::feedback::ls(a.json),
        Some(FeedbackAction::Retry { id }) => return crate::feedback::retry(id.as_deref(), a.json),
        None => {}
    }
    let title = a.title.as_deref().context("feedback needs a title: `shunt feedback <title> [--body ...]`")?;
    let body = read_body(a)?;
    crate::feedback::send(&a.kind, title, &body, a.client.as_deref(), a.dry_run, a.json)
}

/// `--body` > `--body-file` > non-tty stdin; a bare interactive
/// call errors naming its options rather than hanging on stdin.
fn read_body(a: &FeedbackArgs) -> Result<String> {
    if let Some(b) = &a.body {
        return Ok(b.clone());
    }
    if let Some(f) = &a.body_file {
        return std::fs::read_to_string(f).with_context(|| format!("cannot read body file {f}"));
    }
    if std::io::stdin().is_terminal() {
        anyhow::bail!("feedback needs a body: --body TEXT, --body-file F, or stdin");
    }
    let mut s = String::new();
    std::io::stdin().read_to_string(&mut s).context("cannot read body from stdin")?;
    anyhow::ensure!(!s.trim().is_empty(), "feedback body is empty");
    Ok(s)
}
