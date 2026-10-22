//! clap command tree and subcommand dispatch (design.md §10).
//! Server subcommands operate the registry/spool on the dev machine;
//! client subcommands are also the SSH wire surface (`--json`).
//! Server handlers dispatch over the ssh/local transports; client
//! handlers run the filesystem queue state machine (design.md §3).

mod agent;
mod args;
mod feedback;
mod queues;
mod server;
mod upgrade;

use anyhow::Result;
use clap::CommandFactory;

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
fn dual_mode<W, L>(client: &Option<String>, root: &Option<String>, queue: &Option<String>, sub: &str, wire: W, local: L) -> Result<()>
where
    W: FnOnce(&str) -> Result<()>,
    L: FnOnce() -> Result<()>,
{
    if queue.is_some() && root.is_some() {
        anyhow::bail!("--queue cannot select a local --root; use -c CLIENT without --root");
    }
    if let Some(name) = resolve_client(client, root)? {
        return wire(&name);
    }
    anyhow::ensure!(queue.is_none(), "--queue requires a server client; use -c CLIENT (without --root/SHUNT_ROOT), or configure default_client");
    if rootless(root) {
        return print_subhelp(sub);
    }
    local()
}

/// Print rows as aligned columns: every column but the last is
/// padded to its widest cell and followed by a two-space gutter.
/// A row shorter than the widest row just ends early.
fn print_cols(rows: &[Vec<String>]) {
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    if cols == 0 {
        return;
    }
    let mut w = vec![0usize; cols - 1];
    for r in rows {
        for (i, c) in r.iter().enumerate().take(cols - 1) {
            w[i] = w[i].max(c.len());
        }
    }
    for r in rows {
        let mut line = String::new();
        for (i, c) in r.iter().enumerate() {
            if i + 1 == r.len() {
                line.push_str(c);
            } else {
                line.push_str(&format!("{c:w$}  ", w = w[i]));
            }
        }
        println!("{line}");
    }
}

pub fn run(cli: Cli) -> Result<()> {
    use crate::server::registry;
    match cli.command {
        Command::Submit(a) => {
            let client = required_client(&a.client)?;
            // --file and the bare positional are two spellings of the
            // same slot (clap already rejects file + trailing argv).
            let file = a.file.as_deref().or(a.file_pos.as_deref());
            let flags = crate::proto::task::TaskFlags { timeout_s: a.timeout, retry: a.retry, retry_delay_s: a.retry_delay_s, gpu_mem_mb: a.gpu_mem_mb, ram_mb: a.ram_mb, cpu_cores: a.cpu_cores, env: crate::proto::task::parse_env_pairs(&a.env)? };
            let id = crate::server::spool::submit_to(&client, a.queue.as_deref(), a.project.as_deref(), a.guard.as_deref(), a.slug.as_deref(), file, &flags, &a.cmd)?;
            println!("spooled {id} (queue number is assigned on delivery; track with `shunt task show {id}`)");
            Ok(())
        }
        Command::Status(a) => {
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
                || agent_status(&a),
            )
        }
        Command::Logs(a) => {
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
                    let tail = logs_wire_tail(&a);
                    let refs: Vec<&str> = tail.iter().map(String::as_str).collect();
                    remote_queue(name, a.queue.as_deref(), &refs, None, false)
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
        Command::Room(c) => cmd_room(&c),
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
                let rows: Vec<Vec<String>> = crate::server::spool::list()?.iter().map(|e| vec![e.id.clone(), e.submitted_at.clone(), e.client.clone(), e.queue.clone(), e.project.clone(), e.meta.slug.clone()]).collect();
                print_cols(&rows);
                Ok(())
            }
            SpoolAction::Remove { task_id } => crate::server::spool::remove(&task_id),
        },

        Command::Task(c) => match c.action {
            TaskAction::Ls { json } => {
                if json {
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
            TaskAction::Show { id, json } => task_show(&id, json),
            TaskAction::Events => {
                print!("{}", crate::server::tasks::events()?);
                Ok(())
            }
        },

        Command::Queue(c) => queues::cmd_queue(&c),

        Command::Client(c) => match c.action {
            ClientAction::Add(a) => client_add(&a.name, a.template.as_deref(), ClientAddOpts { queue: a.queue, host: a.host, port: a.port, user: a.user, key: a.key, password: a.password, qroot: a.qroot, deploy_root: a.deploy_root, local: a.local, expire: a.expire, env_setup: a.env_setup, shell: a.shell, endpoint_rotates: a.endpoint_rotates, bin_path: a.bin_path, shared: a.shared, exit_on_idle: a.exit_on_idle }),
            ClientAction::Rename { old, new } => crate::server::client_ops::rename(&old, &new),
            // Fold the --endpoint-rotates / --no-endpoint-rotates pair
            // into tri-state: Some(true) sets, Some(false) clears,
            // None leaves the row field untouched.
            ClientAction::Update { name, host, port, user, password, endpoint_rotates, no_endpoint_rotates } => crate::server::client_ops::update(
                &name,
                crate::server::client_ops::EndpointUpdate {
                    host,
                    port,
                    user,
                    password,
                    endpoint_rotates: if endpoint_rotates {
                        Some(true)
                    } else if no_endpoint_rotates {
                        Some(false)
                    } else {
                        None
                    },
                },
            ),
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
                let mut rows = Vec::new();
                for d in vendor_template_dirs() {
                    if let Ok(rd) = std::fs::read_dir(&d) {
                        for e in rd.flatten() {
                            if let Some(n) = e.file_name().to_str().and_then(|s| s.strip_suffix(".toml")) {
                                rows.push(vec![n.to_string(), d.display().to_string()]);
                            }
                        }
                    }
                }
                print_cols(&rows);
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
                    if let Ok(client) = registry::load_client(&name) {
                        let queues = registry::queue_names(&client);
                        anyhow::ensure!(queues.len() == 1 && queues[0] == registry::DEFAULT_QUEUE, "localhost has named queues ({}); refusing factory reset because it would erase their configuration without stopping daemons. Run `shunt queue stop NAME -c localhost` for each queue, then back up clients/localhost.toml and reset the row manually after checking its qroots", queues.join(", "));
                    }
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
            ClientAction::Bootstrap { name, bin_path } => {
                let c = registry::load_client(&name)?;
                let t = crate::server::transport::transport_for(&c);
                crate::server::client_ops::bootstrap(t.as_ref(), &name, bin_path.as_deref())
            }
        },

        Command::Project(c) => match c.action {
            ProjectAction::Add { name, repo, desc } => {
                registry::valid_name(&name)?;
                let e = registry::ProjectEntry { name: name.clone(), desc: desc.unwrap_or_default(), repo: repo.unwrap_or_default(), defaults: Default::default(), bind: Vec::new() };
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
                let e = registry::load_project(&name)?;
                println!("{}", toml::to_string_pretty(&e)?);
                Ok(())
            }
            ProjectAction::Remove { name } => registry::remove("projects", &name),
            ProjectAction::Bind { name, client, client_pos, queue, workdir, env } => {
                let client = client.or(client_pos).expect("clap requires a client");
                let queue = registry::load_queue(&client, queue.as_deref())?.name;
                let mut e = registry::load_project(&name)?;
                let mut map = std::collections::BTreeMap::new();
                for kv in &env {
                    let (k, v) = kv.split_once('=').ok_or_else(|| anyhow::anyhow!("--env expects KEY=value, got {kv}"))?;
                    map.insert(k.to_string(), v.to_string());
                }
                e.bind.retain(|b| b.client != client || b.queue != queue);
                e.bind.push(registry::Binding { client, queue, workdir, env: map });
                let p = registry::save_project(&e)?;
                println!("wrote {}", p.display());
                Ok(())
            }
            ProjectAction::Unbind { name, client, client_pos, queue } => {
                let client = client.or(client_pos).expect("clap requires a client");
                let queue = registry::load_queue(&client, queue.as_deref())?.name;
                let mut e = registry::load_project(&name)?;
                e.bind.retain(|b| b.client != client || b.queue != queue);
                let p = registry::save_project(&e)?;
                println!("wrote {}", p.display());
                Ok(())
            }
        },

        Command::Init(a) => {
            if rootless(&a.root) {
                return print_subhelp("init");
            }
            let r = (|| -> Result<std::path::PathBuf> {
                let root = agent_root(&a.root)?;
                crate::agent::queue::init(&root)?;
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
        Command::Daemon(a) => {
            if rootless(&a.root) {
                return print_subhelp("daemon");
            }
            match agent_root(&a.root).and_then(|root| crate::agent::daemon::run(&root)) {
                Ok(()) => Ok(()),
                Err(e) => jfail(a.json, &format!("{e:#}")),
            }
        }
        Command::Start(a) => {
            if rootless(&a.root) {
                return print_subhelp("start");
            }
            match agent_root(&a.root).and_then(|root| crate::agent::daemon::start(&root)) {
                Ok(pid) => {
                    if a.json {
                        jprint(jobj(jfields(serde_json::json!({"pid": pid}))));
                    } else {
                        println!("queue daemon up (pid {pid})");
                    }
                    Ok(())
                }
                Err(e) => jfail(a.json, &format!("{e:#}")),
            }
        }
        Command::Stop(a) => {
            if rootless(&a.root) {
                return print_subhelp("stop");
            }
            match agent_root(&a.root).and_then(|root| crate::agent::daemon::stop(&root)) {
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
            // mktask can render for real: it is pure output. An empty
            // cmd would render `exec ` — a no-op task that exits 0.
            let r = (|| -> Result<String> {
                if a.cmd.is_empty() {
                    anyhow::bail!("missing command after --");
                }
                let meta = crate::proto::task::TaskMeta { id: None, slug: crate::proto::task::sanitize_slug(&a.slug), guard: a.guard, workdir: None, env: crate::proto::task::parse_env_pairs(&a.env)?, cmd: a.cmd, timeout_s: a.timeout, retry: a.retry, retry_delay_s: a.retry_delay_s, gpu_mem_mb: a.gpu_mem_mb, ram_mb: a.ram_mb, cpu_cores: a.cpu_cores };
                if let Some(p) = crate::proto::task::slug_problem(&meta.slug) {
                    anyhow::bail!("bad slug '{}': {p}", a.slug);
                }
                crate::proto::task::validate_meta(&meta)?;
                Ok(crate::agent::exec::render_task(&meta))
            })();
            match r {
                Ok(s) => {
                    print!("{s}");
                    Ok(())
                }
                Err(e) => jfail(a.json, &format!("{e:#}")),
            }
        }
        Command::Cancel(a) => dual_mode(&a.client, &a.root, &a.queue, "cancel", |name| remote_queue(name, a.queue.as_deref(), &["cancel", "--json", "--", &a.pattern], None, true), || agent_cancel(&a)),
        Command::Prio(a) => dual_mode(
            &a.client,
            &a.root,
            &a.queue,
            "prio",
            |name| {
                let n = a.n.to_string();
                remote_queue(name, a.queue.as_deref(), &["prio", "--json", "--", &a.pattern, &n], None, true)
            },
            || agent_prio(&a),
        ),
        Command::Kill(a) => dual_mode(&a.client, &a.root, &a.queue, "kill", |name| remote_queue(name, a.queue.as_deref(), &["kill", "--json", "--", &a.pattern], None, true), || agent_kill(&a)),
        Command::Workers(a) => {
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
                    let tail = workers_wire_tail(&a);
                    let mut refs: Vec<&str> = tail.iter().map(String::as_str).collect();
                    refs.push("--json");
                    remote_queue(name, a.queue.as_deref(), &refs, None, true)
                },
                || agent_workers(&a),
            )
        }
        Command::Shutdown(a) => {
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
                            let client = registry::load_client(name)?;
                            // shared = user-consented mark that the
                            // box hosts other people's work; arming
                            // idle poweroff would kill their jobs.
                            anyhow::ensure!(!client.shared, "client '{name}' is marked shared — idle poweroff refused (clear `shared` in clients/{name}.toml only if the user confirms sole use)");
                            let resolved = registry::resolve_queue(&client, a.queue.as_deref())?;
                            let t = crate::server::transport::transport_for(&client);
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
                || agent_shutdown(&a),
            )
        }
        Command::Probe(a) => {
            let report = crate::agent::probe::collect(&a.deploy_root);
            if a.json {
                jprint(&report);
            } else {
                match serde_json::to_string_pretty(&report) {
                    Ok(s) => println!("{s}"),
                    Err(e) => return jfail(a.json, &format!("{e:#}")),
                }
            }
            Ok(())
        }
        Command::Completions(a) => {
            let mut cmd = Cli::command();
            clap_complete::generate(a.shell, &mut cmd, "shunt", &mut std::io::stdout());
            Ok(())
        }
        Command::Feedback(a) => feedback::cmd_feedback(&a),
        Command::Version(a) => {
            if let Some(name) = &a.client {
                // Forward the --version flag, not a `version`
                // subcommand — remotes older than this feature
                // don't have one.
                let out = remote(name, &["--version"], None, false);
                return out;
            }
            if a.json {
                let sha = env!("SHUNT_GIT_SHA");
                let date = env!("SHUNT_GIT_DATE");
                println!("{}", serde_json::json!({"shunt": env!("SHUNT_VERSION"), "version": env!("CARGO_PKG_VERSION"), "sha": (!sha.is_empty()).then_some(sha), "date": (!date.is_empty()).then_some(date), "protocol": crate::proto::wire::PROTOCOL_VERSION, "os": std::env::consts::OS, "arch": std::env::consts::ARCH}));
            } else {
                println!("shunt {}", env!("SHUNT_VERSION"));
            }
            Ok(())
        }
        Command::Upgrade(a) => upgrade::cmd_upgrade(&a),
    }
}
