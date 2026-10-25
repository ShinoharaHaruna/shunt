//! Dual-mode queue control ops — `cancel`, `prio`, `kill`,
//! `workers`, `shutdown`: `-c`/registry default forwards the op
//! over the wire; a local queue in scope runs the local handler.

use crate::cli::args::{CleanArgs, PatternArgs, PrioArgs, ShutdownAction, ShutdownArgs, WorkersArgs};
use crate::cli::dual_mode;
use crate::cli::local::{agent_cancel, agent_clean, agent_kill, agent_prio, agent_shutdown, agent_workers, workers_wire_tail};
use crate::cli::server::remote_queue;
use crate::server::registry;
use anyhow::Result;

pub fn cancel(a: &PatternArgs) -> Result<()> {
    dual_mode(&a.client, &a.root, &a.queue, "cancel", |name| remote_queue(name, a.queue.as_deref(), &["cancel", "--json", "--", &a.pattern], None, true), || agent_cancel(a))
}

pub fn prio(a: &PrioArgs) -> Result<()> {
    dual_mode(
        &a.client,
        &a.root,
        &a.queue,
        "prio",
        |name| {
            let n = a.n.to_string();
            remote_queue(name, a.queue.as_deref(), &["prio", "--json", "--", &a.pattern, &n], None, true)
        },
        || agent_prio(a),
    )
}

pub fn kill(a: &PatternArgs) -> Result<()> {
    dual_mode(&a.client, &a.root, &a.queue, "kill", |name| remote_queue(name, a.queue.as_deref(), &["kill", "--json", "--", &a.pattern], None, true), || agent_kill(a))
}

pub fn clean(a: &CleanArgs) -> Result<()> {
    dual_mode(
        &a.client,
        &a.root,
        &a.queue,
        "clean",
        |name| {
            // Forward the selection verbatim — the remote resolves
            // --all and the empty-selection refusal itself.
            let mut tail = vec!["clean", "--json"];
            for (flag, set) in [(a.pending, "--pending"), (a.done, "--done"), (a.failed, "--failed"), (a.archive, "--archive"), (a.all, "--all")] {
                if flag {
                    tail.push(set);
                }
            }
            remote_queue(name, a.queue.as_deref(), &tail, None, true)
        },
        || agent_clean(a),
    )
}

pub fn workers(a: &WorkersArgs) -> Result<()> {
    // `workers 3 set 5` parses but is ambiguous — reject
    // rather than silently apply one spelling.
    if a.n.is_some() && a.action.is_some() {
        anyhow::bail!("pass either positional N or `get`/`set N`, not both");
    }
    dual_mode(
        &a.client,
        &a.root,
        &a.queue,
        "workers",
        |name| {
            let tail = workers_wire_tail(a);
            let mut refs: Vec<&str> = tail.iter().map(String::as_str).collect();
            refs.push("--json");
            remote_queue(name, a.queue.as_deref(), &refs, None, true)
        },
        || agent_workers(a),
    )
}

pub fn shutdown(a: &ShutdownArgs) -> Result<()> {
    dual_mode(
        &a.client,
        &a.root,
        &a.queue,
        "shutdown",
        |name| {
            match &a.action {
                ShutdownAction::On { grace_s, cmd } => {
                    // Push the row's shutdown_cmd unless --cmd
                    // overrides it; the file it writes is the
                    // daemon's first-choice poweroff command.
                    let (client, t) = crate::server::transport::client_transport(name)?;
                    // shared = user-consented mark that the
                    // box hosts other people's work; arming
                    // idle poweroff would kill their jobs.
                    anyhow::ensure!(!client.shared, "client '{name}' is marked shared — idle poweroff refused (clear `shared` in clients/{name}.toml only if the user confirms sole use)");
                    let resolved = registry::resolve_queue(&client, a.queue.as_deref())?;
                    crate::server::queue_ops::check_shutdown(t.as_ref(), &client, &resolved.name)?;
                    let g = grace_s.map(|n| n.to_string());
                    // --cmd > row.shutdown_cmd > facts.shutdown_cmd
                    // (the probe guess from §8 detection).
                    let c = cmd.as_deref().or(client.shutdown_cmd.as_deref()).or_else(|| client.facts.get("shutdown_cmd").and_then(|v| v.as_str()));
                    let mut tail: Vec<String> = vec!["shutdown".into(), "on".into()];
                    if let Some(g) = &g {
                        tail.push(g.clone());
                    }
                    if let Some(c) = c {
                        tail.push("--cmd".into());
                        tail.push(c.to_string());
                    }
                    tail.push("--json".into());
                    let refs: Vec<&str> = tail.iter().map(String::as_str).collect();
                    remote_queue(name, a.queue.as_deref(), &refs, None, true)
                }
                ShutdownAction::Off => remote_queue(name, a.queue.as_deref(), &["shutdown", "off", "--json"], None, true),
                ShutdownAction::Status => remote_queue(name, a.queue.as_deref(), &["shutdown", "status", "--json"], None, true),
            }
        },
        || agent_shutdown(a),
    )
}
