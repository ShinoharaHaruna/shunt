//! Queue control ops: `workers` pool size and `shutdown` arming.

use anyhow::Result;

use super::{jfields, jobj, jprint, local_root, wire};
use crate::cli::args::{ShutdownAction, ShutdownArgs, WorkerAction, WorkersArgs};

pub(crate) fn agent_workers(a: &WorkersArgs) -> Result<()> {
    wire(a.json, workers_impl(a))
}

fn workers_impl(a: &WorkersArgs) -> Result<()> {
    let root = local_root(&a.root)?;
    let set_to = match (&a.n, &a.action) {
        (Some(n), _) => Some(*n),
        (None, Some(WorkerAction::Set { n })) => Some(*n),
        _ => None,
    };
    if let Some(n) = set_to {
        // tmp+rename (umask flavor — a qroot is outside §4.1's
        // 0600 scope): a direct truncate+write leaves an empty file
        // mid-write, and a daemon polling then falls back to
        // last-known workers for one cycle.
        crate::server::fsutil::atomic_write_umask(&root.join("workers"), format!("{n}\n"))?;
    }
    let w = crate::client::daemon::resolve_workers(&root);
    if a.json {
        jprint(jobj(jfields(serde_json::json!({"workers": w.value, "source": w.source}))));
    } else {
        println!("workers: {} ({})", w.value, w.source);
    }
    Ok(())
}

pub(crate) fn agent_shutdown(a: &ShutdownArgs) -> Result<()> {
    wire(a.json, shutdown_impl(a))
}

fn shutdown_impl(a: &ShutdownArgs) -> Result<()> {
    let root = local_root(&a.root)?;
    let counts = || -> serde_json::Value {
        serde_json::json!({
            "running": crate::client::queue::running_list(&root).map(|v| v.len()).unwrap_or(0),
            "pending": crate::client::queue::pending_list(&root).map(|v| v.len()).unwrap_or(0),
        })
    };
    match a.action {
        ShutdownAction::On { grace_s, ref cmd } => {
            if let Some(c) = cmd {
                crate::client::shutdown::set_cmd(&root, c.as_str())?;
            }
            crate::client::shutdown::arm(&root, grace_s)?;
            let st = crate::client::shutdown::status(&root)?;
            if a.json {
                let mut f = serde_json::json!({"armed": true, "grace_s": st.grace_s, "cmd": st.cmd});
                f.as_object_mut().unwrap().extend(counts().as_object().unwrap().clone());
                jprint(jobj(jfields(f)));
            } else {
                println!("armed: shutdown_on_idle set; poweroff fires {}s after pending/ AND running/ are both empty (cmd: {})", st.grace_s, st.cmd);
            }
        }
        ShutdownAction::Off => {
            crate::client::shutdown::disarm(&root)?;
            if a.json {
                let mut f = serde_json::json!({"armed": false});
                f.as_object_mut().unwrap().extend(counts().as_object().unwrap().clone());
                jprint(jobj(jfields(f)));
            } else {
                println!("disarmed: shutdown_on_idle removed (a running countdown cancels within 1 s)");
            }
        }
        ShutdownAction::Status => {
            let st = crate::client::shutdown::status(&root)?;
            if a.json {
                let mut f = serde_json::json!({"armed": st.armed, "grace_s": st.grace_s, "cmd": st.cmd});
                f.as_object_mut().unwrap().extend(counts().as_object().unwrap().clone());
                jprint(jobj(jfields(f)));
            } else if st.armed {
                println!("shutdown_on_idle: ARMED (grace {}s, cmd: {})", st.grace_s, st.cmd);
            } else {
                println!("shutdown_on_idle: off   (arm: shunt shutdown on [grace_s])");
            }
            if !a.json {
                let c = counts();
                println!("queue now: {} running, {} pending", c["running"], c["pending"]);
            }
        }
    }
    Ok(())
}

/// Wire argv for a remote `workers`: `workers set N` when either
/// spelling asks for a set (positional `N` or `set {N}` — same
/// resolution as `agent_workers`), else `workers get`. Without
/// this, `workers -c box set 5` would send `get` and report the
/// old value as if the set had landed.
pub(crate) fn workers_wire_tail(a: &WorkersArgs) -> Vec<String> {
    let set_to = match (&a.n, &a.action) {
        (Some(n), _) => Some(*n),
        (None, Some(WorkerAction::Set { n })) => Some(*n),
        _ => None,
    };
    match set_to {
        Some(n) => vec!["workers".into(), "set".into(), n.to_string()],
        None => vec!["workers".into(), "get".into()],
    }
}
