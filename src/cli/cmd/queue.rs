//! Local queue daemon lifecycle: `init`, `daemon`, `start`,
//! `stop`, `restart` — the §3.5a handoff surfaces on `restart`.
//! These double as the SSH wire surface via --json.

use crate::cli::args::RootArgs;
use crate::cli::local::{jfail, jfields, jobj, jprint, local_root};
use crate::cli::server::{print_subhelp, rootless};
use anyhow::Result;

pub fn init(a: &RootArgs) -> Result<()> {
    if rootless(&a.root) {
        return print_subhelp("init");
    }
    let r = (|| -> Result<std::path::PathBuf> {
        let root = local_root(&a.root)?;
        crate::client::queue::init(&root)?;
        Ok(root)
    })();
    match r {
        Ok(root) => {
            if a.json {
                jprint(jobj(jfields(serde_json::json!({}))));
            } else {
                println!("initialized queue at {}", root.display());
            }
            Ok(())
        }
        Err(e) => jfail(a.json, &format!("{e:#}")),
    }
}

pub fn daemon(a: &RootArgs) -> Result<()> {
    if rootless(&a.root) {
        return print_subhelp("daemon");
    }
    match local_root(&a.root).and_then(|root| crate::client::daemon::run(&root)) {
        Ok(()) => Ok(()),
        Err(e) => jfail(a.json, &format!("{e:#}")),
    }
}

pub fn start(a: &RootArgs) -> Result<()> {
    if rootless(&a.root) {
        return print_subhelp("start");
    }
    match local_root(&a.root).and_then(|root| crate::client::daemon::start(&root)) {
        Ok(crate::client::daemon::StartOutcome::Running { pid }) => {
            if a.json {
                jprint(jobj(jfields(serde_json::json!({"pid": pid}))));
            } else {
                println!("queue daemon up (pid {pid})");
            }
            Ok(())
        }
        Ok(crate::client::daemon::StartOutcome::ExitedIdle) => {
            if a.json {
                jprint(jobj(jfields(serde_json::json!({"started": true, "exited_idle": true}))));
            } else {
                println!("queue daemon started and exited cleanly on idle (exit_on_idle)");
            }
            Ok(())
        }
        Err(e) => jfail(a.json, &format!("{e:#}")),
    }
}

pub fn stop(a: &RootArgs) -> Result<()> {
    if rootless(&a.root) {
        return print_subhelp("stop");
    }
    match local_root(&a.root).and_then(|root| crate::client::daemon::stop(&root)) {
        Ok(()) => {
            if a.json {
                jprint(jobj(jfields(serde_json::json!({}))));
            } else {
                println!("STOP placed; daemon exits after the current task, or at the next poll if idle");
            }
            Ok(())
        }
        Err(e) => jfail(a.json, &format!("{e:#}")),
    }
}

pub fn restart(a: &RootArgs) -> Result<()> {
    if rootless(&a.root) {
        return print_subhelp("restart");
    }
    match local_root(&a.root).and_then(|root| crate::client::daemon::restart(&root)) {
        Ok(crate::client::daemon::Restart::HandedOff { pid, from_ver }) => {
            if a.json {
                jprint(jobj(jfields(serde_json::json!({"pid": pid, "handed_off": true, "from_ver": from_ver}))));
            } else {
                println!("daemon (pid {pid}) re-execs in place — was {from_ver}; in-flight tasks keep running and are adopted by the new image");
            }
            Ok(())
        }
        Ok(crate::client::daemon::Restart::Started { pid }) => {
            if a.json {
                jprint(jobj(jfields(serde_json::json!({"pid": pid, "started": true}))));
            } else {
                println!("no live daemon — started fresh (pid {pid})");
            }
            Ok(())
        }
        Ok(crate::client::daemon::Restart::ExitedIdle) => {
            if a.json {
                jprint(jobj(jfields(serde_json::json!({"started": true, "exited_idle": true}))));
            } else {
                println!("no live daemon — started and exited cleanly on idle (exit_on_idle)");
            }
            Ok(())
        }
        Err(e) => jfail(a.json, &format!("{e:#}")),
    }
}
