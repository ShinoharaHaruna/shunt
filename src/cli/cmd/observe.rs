//! Read-only views: `status`, `logs`, `dlog` — dual-mode, so `-c`
//! forwards over the wire while a local queue in scope runs the
//! local handler.

use crate::cli::args::{LogsArgs, RootArgs, StatusArgs};
use crate::cli::dual_mode;
use crate::cli::local::{agent_dlog, agent_logs, agent_status, logs_wire_tail};
use crate::cli::server::{print_subhelp, remote_queue, rootless};
use anyhow::Result;

pub fn status(a: &StatusArgs) -> Result<()> {
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

pub fn dlog(a: &RootArgs) -> Result<()> {
    if rootless(&a.root) {
        return print_subhelp("dlog");
    }
    agent_dlog(a, None)
}
