//! clap command tree and subcommand dispatch (design.md §10).
//! Server subcommands operate the registry/spool on the dev machine;
//! client subcommands are also the SSH wire surface (`--json`).
//! Registry mutations are real; everything else prints parsed intent
//! and reports itself unimplemented.

mod agent;
mod args;
mod server;

use anyhow::Result;

pub use args::{Cli, Command};

use agent::*;
use args::*;
use server::*;

/// Shared skeleton for dual-mode commands (`status`, `logs`,
/// `cancel`, `prio`, `kill`, `workers`, `shutdown`): `-c`/registry
/// default_client forwards the op over the wire; a local queue in
/// scope runs the agent handler; neither prints subcommand help.
/// `wire` builds the remote argv lazily so the local path never
/// loads the client row (shutdown's arm path reads it).
fn dual_mode<W, L>(client: &Option<String>, root: &Option<String>, sub: &str, wire: W, local: L) -> Result<()>
where
    W: FnOnce(&str) -> Result<()>,
    L: FnOnce() -> Result<()>,
{
    if let Some(name) = resolve_client(client, root)? {
        return wire(&name);
    }
    if rootless(root) {
        return print_subhelp(sub);
    }
    local()
}

pub fn run(cli: Cli) -> Result<()> {
    use crate::server::registry;
    match cli.command {
        Command::Submit(a) => {
            let client = required_client(&a.client)?;
            let id = crate::server::spool::submit(&client, a.project.as_deref(), a.guard.as_deref(), a.slug.as_deref(), a.file.as_deref(), a.timeout, &a.cmd)?;
            println!("spooled {id}");
            Ok(())
        }
        Command::Status(a) => {
            if a.all {
                return crate::server::status::run_all(a.json);
            }
            dual_mode(&a.client, &a.root, "status", |name| remote(name, &["status", "--json"], None), || agent_status(&a))
        }
        Command::Logs(a) => {
            dual_mode(
                &a.client,
                &a.root,
                "logs",
                |name| {
                    // ssh exec is output-collected under a hard
                    // deadline (§5): a follow would buffer silently
                    // until the kill. Say so instead of swallowing -f.
                    if a.follow {
                        anyhow::bail!("`logs -f` cannot follow over the wire; use -n/--tail and re-run, or `shunt client exec {name} -- tail -f <log>`");
                    }
                    let tail = logs_wire_tail(&a);
                    let refs: Vec<&str> = tail.iter().map(String::as_str).collect();
                    remote(name, &refs, None)
                },
                || agent_logs(&a),
            )
        }
        Command::Dlog(a) => {
            if rootless(&a.root) {
                return print_subhelp("dlog");
            }
            agent_dlog(&a, None)
        }
        Command::Dispatch(a) => crate::server::dispatch::run(a.once),
        Command::Tunnel(a) => {
            use crate::server::tunnel;
            let name = required_client(&a.client)?;
            match &a.action {
                TunnelAction::Up { port } => {
                    let c = registry::load_client(&name)?;
                    let t = crate::server::transport::transport_for(&c);
                    let pid = tunnel::up(t.as_ref(), &c, *port)?;
                    println!("tunnel up: {} pid {pid}", c.name);
                    Ok(())
                }
                TunnelAction::Down => {
                    let c = registry::load_client(&name)?;
                    let t = crate::server::transport::transport_for(&c);
                    tunnel::down(t.as_ref(), &c)?;
                    println!("tunnel down: {}", c.name);
                    Ok(())
                }
                TunnelAction::Status => {
                    let st = tunnel::status(&name)?;
                    if st.alive {
                        println!("{name}: up (socks5h://127.0.0.1:{})", st.port.map(|p| p.to_string()).unwrap_or_else(|| "?".into()));
                    } else if st.stale {
                        println!("{name}: down (stale state — the box's env may still export ALL_PROXY; `tunnel up` or `down` cleans it)");
                    } else {
                        println!("{name}: down");
                    }
                    Ok(())
                }
            }
        }

        Command::Spool(c) => match c.action {
            SpoolAction::Ls => {
                for e in crate::server::spool::list()? {
                    println!("{}\t{}\t{}\t{}\t{}", e.id, e.submitted_at, e.client, e.project, e.meta.slug);
                }
                Ok(())
            }
            SpoolAction::Remove { task_id } => crate::server::spool::remove(&task_id),
        },

        Command::Task(c) => match c.action {
            TaskAction::Ls { json } => {
                for r in crate::server::tasks::list()? {
                    if json {
                        println!("{}", serde_json::to_string(&r)?);
                    } else {
                        let state = r.outcome.as_ref().map(|o| format!("{} rc={}", o.state, o.rc)).unwrap_or_else(|| "-".into());
                        println!("{}\t{}\t{}\t{}\t{}\t{}", r.id, r.client, r.queue_name, r.project, r.pushed_at, state);
                    }
                }
                Ok(())
            }
            TaskAction::Show { id, json } => task_show(&id, json),
            TaskAction::Events => {
                print!("{}", crate::server::tasks::events()?);
                Ok(())
            }
        },

        Command::Client(c) => match c.action {
            ClientAction::Add(a) => client_add(&a.name, a.template.as_deref(), ClientAddOpts { host: a.host, port: a.port, user: a.user, key: a.key, password: a.password, qroot: a.qroot, deploy_root: a.deploy_root, local: a.local, expire: a.expire, env_setup: a.env_setup, shell: a.shell, endpoint_rotates: a.endpoint_rotates }),
            ClientAction::Rename { old, new } => crate::server::client_ops::rename(&old, &new),
            ClientAction::Update { name, host, port, user, password, endpoint_rotates } => crate::server::client_ops::update(&name, crate::server::client_ops::EndpointUpdate { host, port, user, password, endpoint_rotates }),
            ClientAction::Exec { name, cmd } => {
                let c = registry::load_client(&name)?;
                let t = crate::server::transport::transport_for(&c);
                let out = t.exec(&c, &cmd, None)?;
                print!("{out}");
                Ok(())
            }
            ClientAction::Hostkey { name } => {
                let c = registry::load_client(&name)?;
                crate::server::client_ops::rekey(&c)?;
                println!("host key dropped: {}", name);
                Ok(())
            }
            ClientAction::Templates => {
                for d in vendor_template_dirs() {
                    if let Ok(rd) = std::fs::read_dir(&d) {
                        for e in rd.flatten() {
                            if let Some(n) = e.file_name().to_str().and_then(|s| s.strip_suffix(".toml")) {
                                println!("{n}\t{}", d.display());
                            }
                        }
                    }
                }
                Ok(())
            }
            ClientAction::Ls => {
                for n in registry::list_clients()? {
                    println!("{n}");
                }
                Ok(())
            }
            ClientAction::Show { name } => {
                let e = registry::load_client(&name)?;
                println!("{}", toml::to_string_pretty(&e)?);
                Ok(())
            }
            ClientAction::Remove { name, purge } => {
                if name == "localhost" {
                    // rm on the reserved loopback = factory reset:
                    // materialize the row, then delete it — the
                    // defaults regenerate on next use (this is also
                    // how a disabled localhost row comes back).
                    let _ = registry::load_client(&name);
                    return registry::remove("clients", &name);
                }
                crate::server::client_ops::remove(&name, purge)
            }
            ClientAction::Probe { name } => {
                let c = registry::load_client(&name)?;
                let t = crate::server::transport::transport_for(&c);
                crate::server::client_ops::probe(t.as_ref(), &name)
            }
            ClientAction::Env { name } => {
                let c = registry::load_client(&name)?;
                let t = crate::server::transport::transport_for(&c);
                crate::server::client_ops::push_env(t.as_ref(), &name)?;
                println!("env pushed: {}", name);
                Ok(())
            }
            ClientAction::Bootstrap { name } => {
                let c = registry::load_client(&name)?;
                let t = crate::server::transport::transport_for(&c);
                crate::server::client_ops::bootstrap(t.as_ref(), &name)
            }
        },

        Command::Project(c) => match c.action {
            ProjectAction::Add { name, repo, desc } => {
                let e = registry::ProjectEntry { name: name.clone(), desc: desc.unwrap_or_default(), repo: repo.unwrap_or_default(), defaults: Default::default(), bind: Vec::new() };
                let p = registry::save_project(&e)?;
                println!("wrote {}", p.display());
                Ok(())
            }
            ProjectAction::Ls => {
                for n in registry::list_names("projects")? {
                    let d = registry::load_project(&n).map(|e| e.desc).unwrap_or_default();
                    if d.is_empty() {
                        println!("{n}");
                    } else {
                        println!("{n}\t{d}");
                    }
                }
                Ok(())
            }
            ProjectAction::Show { name } => {
                let e = registry::load_project(&name)?;
                println!("{}", toml::to_string_pretty(&e)?);
                Ok(())
            }
            ProjectAction::Remove { name } => registry::remove("projects", &name),
            ProjectAction::Bind { name, client, workdir, env } => {
                let mut e = registry::load_project(&name)?;
                let mut map = std::collections::BTreeMap::new();
                for kv in &env {
                    let (k, v) = kv.split_once('=').ok_or_else(|| anyhow::anyhow!("--env expects KEY=value, got {kv}"))?;
                    map.insert(k.to_string(), v.to_string());
                }
                e.bind.retain(|b| b.client != client);
                e.bind.push(registry::Binding { client, workdir, env: map });
                let p = registry::save_project(&e)?;
                println!("wrote {}", p.display());
                Ok(())
            }
            ProjectAction::Unbind { name, client } => {
                let mut e = registry::load_project(&name)?;
                e.bind.retain(|b| b.client != client);
                let p = registry::save_project(&e)?;
                println!("wrote {}", p.display());
                Ok(())
            }
        },

        Command::Init(a) => {
            if rootless(&a.root) {
                return print_subhelp("init");
            }
            let root = agent_root(&a.root)?;
            crate::agent::queue::init(&root)?;
            if a.json {
                jprint(serde_json::json!({"ok": true}));
            } else {
                println!("initialized queue at {}", root.display());
            }
            Ok(())
        }
        Command::Daemon(a) => {
            if rootless(&a.root) {
                return print_subhelp("daemon");
            }
            let root = agent_root(&a.root)?;
            crate::agent::daemon::run(&root)
        }
        Command::Start(a) => {
            if rootless(&a.root) {
                return print_subhelp("start");
            }
            let root = agent_root(&a.root)?;
            match crate::agent::daemon::start(&root) {
                Ok(pid) => {
                    if a.json {
                        jprint(serde_json::json!({"ok": true, "pid": pid}));
                    } else {
                        println!("queue daemon up (pid {pid})");
                    }
                    Ok(())
                }
                Err(e) => jfail(a.json, &e.to_string()),
            }
        }
        Command::Stop(a) => {
            if rootless(&a.root) {
                return print_subhelp("stop");
            }
            let root = agent_root(&a.root)?;
            crate::agent::daemon::stop(&root)?;
            if a.json {
                jprint(serde_json::json!({"ok": true}));
            } else {
                println!("STOP placed; daemon exits after the current task, or at the next poll if idle");
            }
            Ok(())
        }
        Command::Enqueue(a) => {
            if rootless(&a.root) {
                return print_subhelp("enqueue");
            }
            agent_enqueue(&a)
        }
        Command::New(a) => {
            if rootless(&a.root) {
                return print_subhelp("new");
            }
            agent_new(&a)
        }
        Command::Mktask(a) => {
            // mktask can render for real: it is pure output.
            let meta = crate::proto::task::TaskMeta { slug: crate::proto::task::sanitize_slug(&a.slug), guard: a.guard, workdir: None, env: Default::default(), cmd: a.cmd, timeout_s: a.timeout };
            if let Some(p) = crate::proto::task::slug_problem(&meta.slug) {
                anyhow::bail!("bad slug '{}': {p}", a.slug);
            }
            print!("{}", crate::agent::exec::render_task(&meta));
            Ok(())
        }
        Command::Cancel(a) => dual_mode(&a.client, &a.root, "cancel", |name| remote(name, &["cancel", "--json", "--", &a.pattern], None), || agent_cancel(&a)),
        Command::Prio(a) => dual_mode(
            &a.client,
            &a.root,
            "prio",
            |name| {
                let n = a.n.to_string();
                remote(name, &["prio", "--json", "--", &a.pattern, &n], None)
            },
            || agent_prio(&a),
        ),
        Command::Kill(a) => dual_mode(&a.client, &a.root, "kill", |name| remote(name, &["kill", "--json", "--", &a.pattern], None), || agent_kill(&a)),
        Command::Workers(a) => dual_mode(
            &a.client,
            &a.root,
            "workers",
            |name| match a.n {
                Some(n) => remote(name, &["workers", "set", &n.to_string(), "--json"], None),
                None => remote(name, &["workers", "get", "--json"], None),
            },
            || agent_workers(&a),
        ),
        Command::Shutdown(a) => {
            dual_mode(
                &a.client,
                &a.root,
                "shutdown",
                |name| {
                    match &a.action {
                        ShutdownAction::On { grace_s, cmd } => {
                            // Push the row's shutdown_cmd unless --cmd
                            // overrides it; the file it writes is the
                            // daemon's first-choice poweroff command.
                            let client = registry::load_client(name)?;
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
                            remote(name, &refs, None)
                        }
                        ShutdownAction::Off => remote(name, &["shutdown", "off", "--json"], None),
                        ShutdownAction::Status => remote(name, &["shutdown", "status", "--json"], None),
                    }
                },
                || agent_shutdown(&a),
            )
        }
        Command::Probe(a) => {
            let report = crate::agent::probe::collect(&a.deploy_root);
            if a.json {
                jprint(&report);
            } else {
                println!("{}", serde_json::to_string_pretty(&report)?);
            }
            Ok(())
        }
    }
}
