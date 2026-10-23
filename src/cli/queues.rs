//! Named queue configuration and queue-scoped lifecycle commands.

use super::args::{QueueAction, QueueCmd};
use super::server::{remote_queue, required_client};
use crate::server::{client_ops, registry, transport};
use anyhow::Result;

pub(crate) fn cmd_queue(c: &QueueCmd) -> Result<()> {
    match run(c) {
        Ok(()) => Ok(()),
        Err(e) => super::local::jfail(c.json, &format!("{e:#}")),
    }
}

fn run(c: &QueueCmd) -> Result<()> {
    let client_name = required_client(&c.client)?;
    match &c.action {
        QueueAction::Add { name, qroot, env_setup, shell, exit_on_idle, workers } => {
            anyhow::ensure!(*workers != Some(0), "workers must be positive; use --workers 1 or greater");
            let entry = registry::QueueEntry { qroot: qroot.clone(), env_setup: env_setup.clone(), shell: shell.clone(), exit_on_idle: *exit_on_idle, workers: *workers };
            let path = registry::add_queue(&client_name, name, entry)?;
            success(c.json, &client_name, name, "added", Some(&path));
        }
        QueueAction::Default { name } => {
            let path = registry::set_default_queue(&client_name, name)?;
            success(c.json, &client_name, name, "default", Some(&path));
        }
        QueueAction::Ls => {
            let client = registry::load_client(&client_name)?;
            let queues = registry::all_queues(&client)?;
            let views: Vec<_> = queues.iter().map(queue_view).collect();
            if c.json {
                println!("{}", serde_json::json!({"ok": true, "client": client_name, "default_queue": client.default_queue, "queues": views}));
            } else {
                let mut rows = vec![vec!["queue".into(), "qroot".into(), "default".into(), "workers".into()]];
                for q in &queues {
                    rows.push(vec![q.name.clone(), q.client.qroot.clone(), if client.default_queue.as_deref() == Some(&q.name) { "yes".into() } else { "-".into() }, q.workers.map(|n| n.to_string()).unwrap_or_else(|| "runtime".into())]);
                }
                super::print_cols(&rows);
            }
        }
        QueueAction::Show { name } => {
            let queue = registry::load_queue(&client_name, Some(name))?;
            let view = queue_view(&queue);
            if c.json {
                println!("{}", serde_json::json!({"ok": true, "client": client_name, "queue": view}));
            } else {
                println!("{}", serde_json::to_string_pretty(&view)?);
            }
        }
        QueueAction::Start { name } => return remote_queue(&client_name, Some(name), &["start", "--json"], None, true),
        QueueAction::Stop { name } => return remote_queue(&client_name, Some(name), &["stop", "--json"], None, true),
        QueueAction::Restart { name } => return remote_queue(&client_name, Some(name), &["restart", "--json"], None, true),
        QueueAction::Rm { name, purge } => {
            let (_, t) = transport::client_transport(&client_name)?;
            crate::server::queue_ops::remove(t.as_ref(), &client_name, name, *purge)?;
            success(c.json, &client_name, name, "removed", None);
        }
        QueueAction::Bootstrap { name, bin_path } => {
            let (_, t) = transport::client_transport(&client_name)?;
            client_ops::bootstrap_queue(t.as_ref(), &client_name, name, bin_path.as_deref())?;
            success(c.json, &client_name, name, "bootstrapped", None);
        }
        QueueAction::Env { name } => {
            let (_, t) = transport::client_transport(&client_name)?;
            client_ops::push_queue_env(t.as_ref(), &client_name, name)?;
            success(c.json, &client_name, name, "env-pushed", None);
        }
    }
    Ok(())
}

fn queue_view(q: &registry::ResolvedQueue) -> serde_json::Value {
    serde_json::json!({"name": q.name, "qroot": q.client.qroot, "env_setup": q.client.env_setup, "shell": q.client.shell, "exit_on_idle": q.client.exit_on_idle, "workers": q.workers})
}

fn success(json: bool, client: &str, queue: &str, action: &str, path: Option<&std::path::Path>) {
    if json {
        println!("{}", serde_json::json!({"ok": true, "client": client, "queue": queue, "action": action, "path": path.map(|p| p.display().to_string())}));
    } else if let Some(path) = path {
        println!("{client}/{queue}: {action}; wrote {}", path.display());
    } else {
        println!("{client}/{queue}: {action}");
    }
}
