//! Server task pipeline: `submit` spools a task for a client,
//! `spool` manages the staging queue, `task` reads the ledger
//! (design.md §4.5).

use crate::cli::args::{SpoolAction, SpoolCmd, SubmitArgs, TaskAction, TaskCmd};
use crate::cli::print_cols;
use crate::cli::server::{required_client, task_show};
use anyhow::Result;

pub fn submit(a: &SubmitArgs) -> Result<()> {
    let client = required_client(&a.client)?;
    // --file and the bare positional are two spellings of the
    // same slot (clap already rejects file + trailing argv).
    let file = a.file.as_deref().or(a.file_pos.as_deref());
    let flags = crate::proto::task::TaskFlags { timeout_s: a.timeout, retry: a.retry, retry_delay_s: a.retry_delay_s, gpu_mem_mb: a.gpu_mem_mb, ram_mb: a.ram_mb, cpu_cores: a.cpu_cores, env: crate::proto::task::parse_env_pairs(&a.env)? };
    let id = crate::server::spool::submit_to(&client, a.queue.as_deref(), a.project.as_deref(), a.guard.as_deref(), a.slug.as_deref(), file, &flags, a.hold.clone(), a.hold_on_box.clone(), &a.cmd)?;
    if a.hold.is_some() {
        let box_note = a.hold_on_box.as_ref().map(|reason| if reason.is_empty() { "; it will also be held on the box after spool release".to_string() } else { format!("; it will also be held on the box: {reason}") }).unwrap_or_default();
        println!("spooled {id} HELD — dispatch skips it until `shunt spool release {id}`{box_note}");
    } else if let Some(reason) = &a.hold_on_box {
        println!("spooled {id} (dispatcher will deliver it held on the box{}; track with `shunt task show {id}`)", if reason.is_empty() { String::new() } else { format!(": {reason}") });
    } else {
        println!("spooled {id} (queue number is assigned on delivery; track with `shunt task show {id}`)");
    }
    Ok(())
}

pub fn spool(c: &SpoolCmd) -> Result<()> {
    match &c.action {
        SpoolAction::Ls => {
            let rows: Vec<Vec<String>> = crate::server::spool::list()?
                .iter()
                .map(|e| {
                    let hold = e.hold.as_ref().map(|r| if r.is_empty() { "held in spool".into() } else { format!("held in spool: {r}") }).unwrap_or_else(|| "-".into());
                    let box_hold = e.hold_on_box.as_ref().map(|r| if r.is_empty() { "held on box".into() } else { format!("held on box: {r}") }).unwrap_or_else(|| "-".into());
                    vec![e.id.clone(), e.submitted_at.clone(), e.client.clone(), e.queue.clone(), e.project.clone(), e.meta.slug.clone(), hold, box_hold]
                })
                .collect();
            print_cols(&rows);
            Ok(())
        }
        SpoolAction::Remove { task_id } => crate::server::spool::remove(task_id),
        SpoolAction::Hold { task_id, reason } => {
            let id = crate::server::spool::set_hold(task_id, Some(reason.clone().unwrap_or_default()))?;
            println!("{id} held — dispatch skips it until `shunt spool release {id}`");
            Ok(())
        }
        SpoolAction::Release { task_id } => {
            let id = crate::server::spool::set_hold(task_id, None)?;
            println!("{id} released — the next dispatch sweep will deliver it");
            Ok(())
        }
    }
}

pub fn task(c: &TaskCmd) -> Result<()> {
    match &c.action {
        TaskAction::Ls { json } => {
            if *json {
                for r in crate::server::tasks::list()? {
                    println!("{}", serde_json::to_string(&r)?);
                }
            } else {
                let rows: Vec<Vec<String>> = crate::server::tasks::list()?
                    .iter()
                    .map(|r| {
                        let state = r.outcome.as_ref().map(|o| format!("{} rc={}", o.state, o.rc)).unwrap_or_else(|| "-".into());
                        vec![r.id.clone(), r.client.clone(), r.queue.clone(), r.queue_name.clone(), r.project.clone(), r.pushed_at.clone(), state]
                    })
                    .collect();
                print_cols(&rows);
            }
            Ok(())
        }
        TaskAction::Show { id, json } => task_show(id, *json),
        TaskAction::Events => {
            print!("{}", crate::server::tasks::events()?);
            Ok(())
        }
    }
}
