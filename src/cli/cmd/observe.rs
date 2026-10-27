//! Read-only views: `status`, `logs`, `dlog` — dual-mode, so `-c`
//! forwards over the wire while a local queue in scope runs the
//! local handler.

use crate::cli::args::{CatArgs, LogsArgs, RootArgs, StatusArgs};
use crate::cli::dual_mode;
use crate::cli::local::{agent_cat, agent_dlog, agent_logs, agent_status, logs_wire_tail};
use crate::cli::server::{print_subhelp, remote_queue, rootless};
use anyhow::Result;

pub fn status(a: &StatusArgs) -> Result<()> {
    // Ordinary client resolution excludes disabled rows. Status is the
    // exception: it can report the registry state without contacting them.
    if !a.all && a.as_label.is_none() && a.queue.is_none() {
        if let Some(typed) = a.client.as_deref() {
            use crate::server::registry::names::Resolution;
            let disabled = match crate::server::registry::names::resolve(typed, &crate::server::registry::list_names("clients")?) {
                Resolution::Exact(name) | Resolution::Canonical { canonical: name, .. } if crate::server::registry::load_client_row(&name).is_ok_and(|row| row.disabled) => Some(name),
                _ => None,
            };
            if let Some(name) = disabled {
                if name != typed {
                    eprintln!("note: resolved '{typed}' → disabled client '{name}'");
                }
                return crate::server::status::run_client(&name, a.json);
            }
        }
    }
    // --as only makes sense on the server aggregate (§17.5:
    // the rooms block); its presence implies --all. clap
    // already rejects combining it with -c/--root.
    if a.all || a.as_label.is_some() {
        return crate::server::status::run_all(a.json, a.as_label.as_deref());
    }
    dual_mode(
        &a.client,
        &a.root,
        &a.queue,
        "status",
        |name| {
            if a.queue.is_none() {
                crate::server::status::run_client(name, a.json)
            } else {
                remote_queue(name, a.queue.as_deref(), &["status", "--json"], None, true)
            }
        },
        || agent_status(a),
    )
}

pub fn logs(a: &LogsArgs) -> Result<()> {
    dual_mode(
        &a.client,
        &a.root,
        &a.queue,
        "logs",
        |name| {
            // ssh exec is output-collected under a hard
            // deadline (§5): a follow would buffer silently
            // until the kill. Say so instead of swallowing -f.
            if a.follow {
                anyhow::bail!("`logs -f` cannot follow over the wire; use -n/--tail and re-run, or `shunt client exec {name} -- tail -f <log>`");
            }
            let tail = logs_wire_tail(a);
            let refs: Vec<&str> = tail.iter().map(String::as_str).collect();
            remote_queue(name, a.queue.as_deref(), &refs, None, false)
        },
        || agent_logs(a),
    )
}

/// `cat` is a text op like `logs`: the remote answers task.sh on
/// stdout, so `expect_json` stays false and the body passes through
/// verbatim — a script line shaped like a wire object is content,
/// never a protocol answer.
pub fn cat(a: &CatArgs) -> Result<()> {
    dual_mode(&a.client, &a.root, &a.queue, "cat", |name| remote_queue(name, a.queue.as_deref(), &["cat", "--", &a.pattern], None, false), || agent_cat(a))
}

pub fn dlog(a: &RootArgs) -> Result<()> {
    if rootless(&a.root) {
        return print_subhelp("dlog");
    }
    agent_dlog(a, None)
}
