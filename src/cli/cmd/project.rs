//! `project` subcommand dispatch: registry CRUD plus the
//! `bind`/`unbind` client-workdir edges rooms coordinate over
//! (design.md §17.4).

use crate::cli::args::{ProjectAction, ProjectCmd};
use crate::cli::print_cols;
use crate::cli::server::canonical_client;
use crate::server::registry;
use anyhow::Result;

pub fn project(c: &ProjectCmd) -> Result<()> {
    match &c.action {
        ProjectAction::Add { name, repo, desc } => {
            registry::valid_name(name)?;
            // `admin` is the room's reserved operator identity
            // (§17.3): a project by that name would collide
            // with it in every --as comparison.
            anyhow::ensure!(!name.eq_ignore_ascii_case(crate::server::room::ADMIN_LABEL), "{name:?} is reserved for the room operator identity (§17.3) — pick a real project name");
            let e = registry::ProjectEntry { name: name.clone(), desc: desc.clone().unwrap_or_default(), repo: repo.clone().unwrap_or_default(), defaults: Default::default(), bind: Vec::new() };
            let p = registry::save_project(&e)?;
            println!("wrote {}", p.display());
            Ok(())
        }
        ProjectAction::Ls => {
            let rows: Vec<Vec<String>> = registry::list_names("projects")?
                .into_iter()
                .map(|n| {
                    let d = registry::load_project(&n).map(|e| e.desc).unwrap_or_default();
                    if d.is_empty() {
                        vec![n]
                    } else {
                        vec![n, d]
                    }
                })
                .collect();
            print_cols(&rows);
            Ok(())
        }
        ProjectAction::Show { name } => {
            let e = registry::load_project(name)?;
            println!("{}", toml::to_string_pretty(&e)?);
            Ok(())
        }
        ProjectAction::Remove { name } => registry::remove("projects", name),
        ProjectAction::Bind { name, client, client_pos, queue, workdir, env } => {
            let client = canonical_client(client.clone().or(client_pos.clone()).as_deref().expect("clap requires a client"))?;
            let queue = registry::load_queue(&client, queue.as_deref())?.name;
            let mut e = registry::load_project(name)?;
            let mut map = std::collections::BTreeMap::new();
            for kv in env {
                let (k, v) = kv.split_once('=').ok_or_else(|| anyhow::anyhow!("--env expects KEY=value, got {kv}"))?;
                map.insert(k.to_string(), v.to_string());
            }
            e.bind.retain(|b| b.client != client || b.queue != queue);
            e.bind.push(registry::Binding { client, queue, workdir: workdir.clone(), env: map });
            let p = registry::save_project(&e)?;
            println!("wrote {}", p.display());
            Ok(())
        }
        ProjectAction::Unbind { name, client, client_pos, queue } => {
            let client = canonical_client(client.clone().or(client_pos.clone()).as_deref().expect("clap requires a client"))?;
            let queue = registry::load_queue(&client, queue.as_deref())?.name;
            let mut e = registry::load_project(name)?;
            e.bind.retain(|b| b.client != client || b.queue != queue);
            let p = registry::save_project(&e)?;
            println!("wrote {}", p.display());
            Ok(())
        }
    }
}
