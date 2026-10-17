//! clap command tree and subcommand dispatch (design.md §10).
//! Server subcommands operate the registry/spool on the dev machine;
//! client subcommands are also the SSH wire surface (`--json`).
//! Registry mutations are real; everything else prints parsed intent
//! and reports itself unimplemented.

use anyhow::{Context as _, Result};
use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "shunt",
    version,
    about = "Experiment queue for hourly-billed GPU boxes: FIFO + at-least-once, per-task process groups, idle auto-shutdown",
    // Single-line long_about: clap wraps help text to $COLUMNS at
    // render time; baked \n would render the same breaks at every
    // terminal width and fight clap's own wrap.
    long_about = "shunt runs on both sides. Server subcommands (submit, client, project, spool, dispatch) operate the registry and staging queue on the dev machine. Client subcommands (daemon, enqueue, status, ...) operate the local queue and double as the SSH wire protocol via --json."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Spool a task for a client (server)
    Submit(SubmitArgs),
    /// Queue status: aggregate (server) or local (client)
    Status(StatusArgs),
    /// Task or daemon logs
    Logs(LogsArgs),
    /// Daemon log only (client convenience)
    Dlog(RootArgs),
    /// Manage the server-side staging queue
    Spool(SpoolCmd),
    /// Manage registered experiment boxes
    Client(ClientCmd),
    /// Manage registered projects and their client bindings
    Project(ProjectCmd),
    /// Run the spool dispatcher loop (server)
    Dispatch(DispatchArgs),
    /// Box-side SOCKS5 tunnel over ssh -R (server)
    Tunnel(TunnelArgs),

    /// Create queue dirs under --root (client)
    Init(RootArgs),
    /// Run the queue daemon in the foreground (client)
    Daemon(RootArgs),
    /// Start the daemon in the background (client)
    Start(RootArgs),
    /// Stop the daemon (client)
    Stop(RootArgs),
    /// Install a task file (or stdin with `-`) into pending/ (client)
    Enqueue(EnqueueArgs),
    /// Wrap argv as a guarded task and enqueue it (client)
    New(NewArgs),
    /// Print the generated task body to stdout (client)
    Mktask(NewArgs),
    /// Remove a pending task by name/pattern
    Cancel(PatternArgs),
    /// Renumber a pending task into the priority band 0000-0099
    Prio(PrioArgs),
    /// Kill a running task's process group
    Kill(PatternArgs),
    /// Get or set the worker pool size
    Workers(WorkersArgs),
    /// Idle-shutdown control (client) or remote arm via -c (server)
    Shutdown(ShutdownArgs),
    /// Print machine facts as JSON (client)
    Probe(ProbeArgs),
}

#[derive(Args)]
pub struct SubmitArgs {
    /// Project name from the registry
    #[arg(short, long)]
    pub project: Option<String>,
    /// Target client name from the registry
    #[arg(short, long)]
    pub client: String,
    /// Guard path: task exits 0 early if this exists at run time
    #[arg(long)]
    pub guard: Option<String>,
    /// Slug override (default: derived from cmd or file)
    #[arg(long)]
    pub slug: Option<String>,
    /// Existing task file to spool instead of wrapping argv
    #[arg(long, conflicts_with = "cmd")]
    pub file: Option<String>,
    /// Command to run, after `--`
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub cmd: Vec<String>,
}

#[derive(Args)]
pub struct StatusArgs {
    /// Aggregate every registered client (server side)
    #[arg(long)]
    pub all: bool,
    /// Query one client over SSH (server side)
    #[arg(short, long, conflicts_with = "all")]
    pub client: Option<String>,
    /// Local queue root (client side)
    #[arg(long)]
    pub root: Option<String>,
    /// Emit a single JSON object (wire protocol)
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct LogsArgs {
    /// Task name/pattern; omitted = daemon log
    pub pattern: Option<String>,
    /// Query a client over SSH (server side)
    #[arg(short, long)]
    pub client: Option<String>,
    /// Local queue root (client side)
    #[arg(long)]
    pub root: Option<String>,
    /// Follow the log (client side; stream, not JSON)
    #[arg(short, long)]
    pub follow: bool,
}

#[derive(Args)]
pub struct RootArgs {
    /// Queue root; falls back to $SHUNT_ROOT
    #[arg(long)]
    pub root: Option<String>,
    /// Emit JSON (wire protocol)
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct EnqueueArgs {
    /// Task file path, or `-` to read the body from stdin (wire form)
    pub file: String,
    /// Slug; required for the stdin form
    pub slug: Option<String>,
    #[arg(long)]
    pub root: Option<String>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct NewArgs {
    /// Task slug (sanitized to [A-Za-z0-9_.-])
    #[arg(long)]
    pub slug: String,
    /// Guard path: skip if it exists at run time
    #[arg(long)]
    pub guard: Option<String>,
    #[arg(long)]
    pub root: Option<String>,
    #[arg(long)]
    pub json: bool,
    /// Command to wrap, after `--`
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub cmd: Vec<String>,
}

#[derive(Args)]
pub struct PatternArgs {
    /// Pending/running task name or unique substring
    pub pattern: String,
    /// Forward to a client over SSH (server side)
    #[arg(short, long)]
    pub client: Option<String>,
    #[arg(long)]
    pub root: Option<String>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct PrioArgs {
    /// Pending task name or unique substring
    pub pattern: String,
    /// New sequence number, normally inside the 0000-0099 band
    pub n: u32,
    /// Forward to a client over SSH (server side)
    #[arg(short, long)]
    pub client: Option<String>,
    #[arg(long)]
    pub root: Option<String>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct WorkersArgs {
    /// New pool size; omitted = report current resolved value
    pub n: Option<u32>,
    /// Wire form: `workers get` / `workers set N`
    #[command(subcommand)]
    pub action: Option<WorkerAction>,
    /// Forward to a client over SSH (server side)
    #[arg(short, long)]
    pub client: Option<String>,
    #[arg(long, global = true)]
    pub root: Option<String>,
    #[arg(long, global = true)]
    pub json: bool,
}

#[derive(Subcommand)]
pub enum WorkerAction {
    /// Report the resolved pool size
    Get,
    /// Set the pool size
    Set {
        /// Pool size
        n: u32,
    },
}

#[derive(Args)]
pub struct ShutdownArgs {
    /// on [grace_s] | off | status
    #[command(subcommand)]
    pub action: ShutdownAction,
    /// Forward to a client over SSH (server side)
    #[arg(short, long)]
    pub client: Option<String>,
    #[arg(long, global = true)]
    pub root: Option<String>,
    #[arg(long, global = true)]
    pub json: bool,
}

#[derive(Subcommand)]
pub enum ShutdownAction {
    /// Arm idle poweroff
    On {
        /// Countdown seconds (default 60)
        grace_s: Option<u64>,
        /// Poweroff command to record as <qroot>/shutdown_cmd before
        /// arming (placeholder commands like `true` for test envs)
        #[arg(long)]
        cmd: Option<String>,
    },
    /// Disarm
    Off,
    /// Report armed state
    Status,
}

#[derive(Args)]
pub struct ProbeArgs {
    /// Deploy root to measure free disk on (default: /)
    #[arg(long, default_value = "/")]
    pub deploy_root: String,
    #[arg(long)]
    pub root: Option<String>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct DispatchArgs {
    /// Single sweep of spool/, then exit (for tests)
    #[arg(long)]
    pub once: bool,
}

#[derive(Args)]
pub struct TunnelArgs {
    #[command(subcommand)]
    pub action: TunnelAction,
    /// Client name
    #[arg(short, long)]
    pub client: String,
}

#[derive(Subcommand)]
pub enum TunnelAction {
    /// Open the reverse tunnel: box-side SOCKS5 on 127.0.0.1:<port>,
    /// exiting via the dev machine's network
    Up {
        /// SOCKS5 listen port on the box (default 1080)
        #[arg(long)]
        port: Option<u16>,
    },
    /// Tear the tunnel down and clean the proxy from the box's env
    Down,
    /// Report liveness and port
    Status,
}

#[derive(Args)]
pub struct SpoolCmd {
    #[command(subcommand)]
    pub action: SpoolAction,
}

#[derive(Subcommand)]
pub enum SpoolAction {
    /// List staged entries oldest-first
    Ls,
    /// Remove a staged entry by task id
    Rm { task_id: String },
}

#[derive(Args)]
pub struct ClientCmd {
    #[command(subcommand)]
    pub action: ClientAction,
}

/// Fields of `client add` — a struct variant would dwarf the enum's
/// other arms (clippy::large_enum_variant), so it is an Args tuple.
#[derive(Args)]
pub struct ClientAddArgs {
    /// Registry name
    pub name: String,
    /// Vendor template under etc/vendors/ (e.g. autodl)
    #[arg(long)]
    pub template: Option<String>,
    /// SSH host
    #[arg(long)]
    pub host: Option<String>,
    /// SSH port
    #[arg(long)]
    pub port: Option<u16>,
    /// SSH user (template default: root)
    #[arg(long)]
    pub user: Option<String>,
    /// SSH private key path (empty = generated at keys/<name>)
    #[arg(long)]
    pub key: Option<String>,
    /// SSH password — enables unattended key deploy on bootstrap
    /// (AutoDL-style password auth)
    #[arg(long)]
    pub password: Option<String>,
    /// Queue dir on the box (template default: deploy_root/queue)
    #[arg(long)]
    pub qroot: Option<String>,
    /// Vendor work root (template default for autodl: /root/autodl-tmp)
    #[arg(long)]
    pub deploy_root: Option<String>,
    /// Local loopback client (no SSH); the built-in `localhost`
    /// row is always local regardless (design.md §4.1, §5)
    #[arg(long)]
    pub local: bool,
    /// Billing lifetime: "unlimited", "metered", or an ISO-8601
    /// timestamp for a subscription deadline
    #[arg(long)]
    pub expire: Option<String>,
    /// Shell snippet sourced before every task on this box
    /// (e.g. "source ~/miniconda3/etc/profile.d/conda.sh && conda activate ribo")
    #[arg(long)]
    pub env_setup: Option<String>,
    /// Shell for task wrappers + env_setup (default bash)
    #[arg(long)]
    pub shell: Option<String>,
    /// Vendor reassigns IP/port across restarts (AutoDL does not)
    #[arg(long)]
    pub endpoint_rotates: bool,
}

// Add is parsed once per invocation — a fat variant is fine and
// clap cannot Box the payload, so silence the lint honestly.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
pub enum ClientAction {
    /// Register a box (server-local, zero network traffic)
    Add(ClientAddArgs),
    /// List registered boxes
    Ls,
    /// Show one client row
    Show { name: String },
    /// Remove a client row
    #[command(alias = "remove")]
    Rm { name: String },
    /// Re-collect machine facts over SSH and merge into the row
    Probe { name: String },
    /// Probe raw SSH, scp the binary, remote init + start
    Bootstrap { name: String },
    /// Push the row's env_setup/shell to <qroot>/env_setup + shell
    Env { name: String },
    /// List vendor templates visible to `client add --template`
    Templates,
    /// Drop the box's host key from shunt's known_hosts + kill the
    /// mux master (use after a container rebuild changed the key)
    Hostkey { name: String },
    /// Rename a client: row + name field + keypair + mux socket +
    /// state + spool refs + project bindings move together
    Rename { old: String, new: String },
}

#[derive(Args)]
pub struct ProjectCmd {
    #[command(subcommand)]
    pub action: ProjectAction,
}

#[derive(Subcommand)]
pub enum ProjectAction {
    /// Register a project
    Add { name: String },
    /// List registered projects
    Ls,
    /// Show one project row
    Show { name: String },
    /// Remove a project row
    #[command(alias = "remove")]
    Rm { name: String },
    /// Bind the project to a client with a workdir + env overlay
    Bind {
        name: String,
        #[arg(long)]
        client: String,
        #[arg(long)]
        workdir: String,
        /// KEY=value env overlay entries (repeatable)
        #[arg(long = "env", value_name = "KEY=value")]
        env: Vec<String>,
    },
    /// Remove a binding
    Unbind {
        name: String,
        #[arg(long)]
        client: String,
    },
}

/// Forward one wire op to a client and print remote stdout.
/// The remote argv is `<qroot>/bin/shunt <tail...> --root <qroot>`;
/// transport is `ssh` or the `localhost` loopback (design.md §5).
fn remote(name: &str, tail: &[&str], stdin: Option<&str>) -> Result<()> {
    use crate::server::registry;
    use crate::server::transport::{transport_for, wire_argv};
    let client = registry::load_client(name)?;
    let argv = wire_argv(&client, tail);
    let t = transport_for(&client);
    let out = t.exec(&client, &argv, stdin)?;
    print!("{out}");
    if !out.ends_with('\n') {
        println!();
    }
    Ok(())
}

pub fn run(cli: Cli) -> Result<()> {
    use crate::server::registry;
    match cli.command {
        Command::Submit(a) => {
            let id = crate::server::spool::submit(&a.client, a.project.as_deref(), a.guard.as_deref(), a.slug.as_deref(), a.file.as_deref(), &a.cmd)?;
            println!("spooled {id}");
            Ok(())
        }
        Command::Status(a) => {
            if let Some(name) = &a.client {
                return remote(name, &["status", "--json"], None);
            }
            if a.all {
                return crate::server::status::run_all(a.json);
            }
            agent_status(&a)
        }
        Command::Logs(a) => {
            if let Some(name) = &a.client {
                let pat = a.pattern.clone().unwrap_or_default();
                return remote(name, &["logs", &pat], None);
            }
            agent_logs(&a)
        }
        Command::Dlog(a) => agent_dlog(&a),
        Command::Dispatch(a) => crate::server::dispatch::run(a.once),
        Command::Tunnel(a) => {
            use crate::server::tunnel;
            match &a.action {
                TunnelAction::Up { port } => {
                    let c = registry::load_client(&a.client)?;
                    let t = crate::server::transport::transport_for(&c);
                    let pid = tunnel::up(t.as_ref(), &c, *port)?;
                    println!("tunnel up: {} pid {pid}", c.name);
                    Ok(())
                }
                TunnelAction::Down => {
                    let c = registry::load_client(&a.client)?;
                    let t = crate::server::transport::transport_for(&c);
                    tunnel::down(t.as_ref(), &c)?;
                    println!("tunnel down: {}", c.name);
                    Ok(())
                }
                TunnelAction::Status => {
                    let st = tunnel::status(&a.client)?;
                    if st.alive {
                        println!("{}: up (socks5h://127.0.0.1:{})", a.client, st.port.map(|p| p.to_string()).unwrap_or_else(|| "?".into()));
                    } else {
                        println!("{}: down", a.client);
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
            SpoolAction::Rm { task_id } => crate::server::spool::remove(&task_id),
        },

        Command::Client(c) => match c.action {
            ClientAction::Add(a) => client_add(&a.name, a.template.as_deref(), ClientAddOpts { host: a.host, port: a.port, user: a.user, key: a.key, password: a.password, qroot: a.qroot, deploy_root: a.deploy_root, local: a.local, expire: a.expire, env_setup: a.env_setup, shell: a.shell, endpoint_rotates: a.endpoint_rotates }),
            ClientAction::Rename { old, new } => crate::server::client_ops::rename(&old, &new),
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
            ClientAction::Rm { name } => {
                // Materialize `localhost` first so `rm` resets it to
                // factory defaults even on a pristine registry — and
                // undeletes a disabled row (it regenerates on next use).
                let _ = registry::load_client(&name);
                registry::remove("clients", &name)
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
            ProjectAction::Add { name } => {
                let e = registry::ProjectEntry { name: name.clone(), bind: Vec::new() };
                let p = registry::save_project(&e)?;
                println!("wrote {}", p.display());
                Ok(())
            }
            ProjectAction::Ls => {
                for n in registry::list_names("projects")? {
                    println!("{n}");
                }
                Ok(())
            }
            ProjectAction::Show { name } => {
                let e = registry::load_project(&name)?;
                println!("{}", toml::to_string_pretty(&e)?);
                Ok(())
            }
            ProjectAction::Rm { name } => registry::remove("projects", &name),
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
            let root = agent_root(&a.root)?;
            crate::agent::daemon::run(&root)
        }
        Command::Start(a) => {
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
            let root = agent_root(&a.root)?;
            crate::agent::daemon::stop(&root)?;
            if a.json {
                jprint(serde_json::json!({"ok": true}));
            } else {
                println!("STOP placed; daemon exits after the current task, or at the next poll if idle");
            }
            Ok(())
        }
        Command::Enqueue(a) => agent_enqueue(&a),
        Command::New(a) => agent_new(&a),
        Command::Mktask(a) => {
            // mktask can render for real: it is pure output.
            let meta = crate::proto::task::TaskMeta { slug: crate::proto::task::sanitize_slug(&a.slug), guard: a.guard, workdir: None, env: Default::default(), cmd: a.cmd };
            if meta.slug.is_empty() {
                anyhow::bail!("empty slug after sanitization");
            }
            print!("{}", crate::agent::exec::render_task(&meta));
            Ok(())
        }
        Command::Cancel(a) => {
            if let Some(name) = &a.client {
                return remote(name, &["cancel", &a.pattern, "--json"], None);
            }
            agent_cancel(&a)
        }
        Command::Prio(a) => {
            if let Some(name) = &a.client {
                let n = a.n.to_string();
                return remote(name, &["prio", &a.pattern, &n, "--json"], None);
            }
            agent_prio(&a)
        }
        Command::Kill(a) => {
            if let Some(name) = &a.client {
                return remote(name, &["kill", &a.pattern, "--json"], None);
            }
            agent_kill(&a)
        }
        Command::Workers(a) => {
            if let Some(name) = &a.client {
                return match a.n {
                    Some(n) => remote(name, &["workers", "set", &n.to_string(), "--json"], None),
                    None => remote(name, &["workers", "get", "--json"], None),
                };
            }
            agent_workers(&a)
        }
        Command::Shutdown(a) => {
            if let Some(name) = &a.client {
                return match a.action {
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
                };
            }
            agent_shutdown(&a)
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

/// `shunt client add`: seeds the row from a vendor template when given,
/// then applies explicit flags. Real (server-local bookkeeping only).
/// Optional field overrides for `client add`.
struct ClientAddOpts {
    host: Option<String>,
    port: Option<u16>,
    user: Option<String>,
    key: Option<String>,
    qroot: Option<String>,
    deploy_root: Option<String>,
    local: bool,
    expire: Option<String>,
    env_setup: Option<String>,
    shell: Option<String>,
    endpoint_rotates: bool,
    password: Option<String>,
}

fn client_add(name: &str, template: Option<&str>, opts: ClientAddOpts) -> Result<()> {
    use crate::server::registry::{ClientEntry, SshEndpoint};

    let mut entry = match template {
        Some(t) => load_vendor_template(t)?,
        None => ClientEntry { name: name.into(), local: false, disabled: false, local_bin: None, timeout_s: None, ssh_timeout_s: None, shutdown_cmd: None, expire: String::new(), env_setup: String::new(), shell: String::new(), endpoint_rotates: false, ssh: SshEndpoint { host: String::new(), port: 22, user: "root".into(), key: String::new(), password: String::new() }, qroot: String::new(), deploy_root: String::new(), facts: Default::default(), billing: None },
    };
    entry.name = name.into();
    // Adding a row for the built-in name materializes the loopback
    // defaults instead of a broken ssh-shaped row (registry §4.1).
    if name == "localhost" {
        entry.local = true;
        if entry.qroot.is_empty() {
            entry.qroot = crate::server::registry::shunt_home()?.join("queue").display().to_string();
        }
        if entry.deploy_root.is_empty() {
            entry.deploy_root = std::env::var("HOME").unwrap_or_default();
        }
    }
    if let Some(v) = opts.host {
        entry.ssh.host = v;
    }
    if let Some(v) = opts.port {
        entry.ssh.port = v;
    }
    if let Some(v) = opts.user {
        entry.ssh.user = v;
    }
    if let Some(v) = opts.key {
        entry.ssh.key = v;
    }
    if let Some(v) = opts.qroot {
        entry.qroot = v;
    }
    if let Some(v) = opts.deploy_root {
        entry.deploy_root = v;
    }
    if opts.local {
        entry.local = true;
    }
    if let Some(v) = opts.expire {
        entry.expire = v;
    }
    if let Some(v) = opts.env_setup {
        entry.env_setup = v;
    }
    if let Some(v) = opts.shell {
        entry.shell = v;
    }
    if let Some(v) = opts.password {
        entry.ssh.password = v;
    }
    if opts.endpoint_rotates {
        entry.endpoint_rotates = true;
    }
    // qroot defaults to <deploy_root>/queue — an empty qroot would
    // make remote ops land in the box's CWD (silent wrongness).
    if entry.qroot.is_empty() && !entry.deploy_root.is_empty() {
        entry.qroot = format!("{}/queue", entry.deploy_root.trim_end_matches('/'));
    }
    // Dedicated keypair per client (design.md §4.1): generated at
    // registration so the row is usable immediately.
    if !entry.local {
        crate::server::client_ops::ensure_key(&mut entry)?;
    }
    let p = crate::server::registry::save_client(&entry)?;
    println!("wrote {}", p.display());
    if !entry.local && (entry.ssh.host.is_empty() || entry.ssh.key.is_empty() || entry.ssh.port == 0) {
        eprintln!("note: ssh fields still incomplete; edit the row before bootstrap");
    }
    Ok(())
}

/// Vendor-template search dirs, first hit wins: `$SHUNT_VENDOR_DIR`,
/// the registry's own `vendors/` (user-supplied templates live with
/// the data they seed), then the repo's `etc/vendors/` — a
/// compile-time path that only works for builds run from the source
/// tree.
fn vendor_template_dirs() -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(d) = std::env::var("SHUNT_VENDOR_DIR") {
        dirs.push(std::path::PathBuf::from(d));
    }
    if let Ok(h) = crate::server::registry::shunt_home() {
        dirs.push(h.join("vendors"));
    }
    dirs.push(std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("etc/vendors"));
    dirs
}

/// Load `<name>.toml` from the vendor-template dirs as a client-row
/// seed. On a miss the error lists the templates that do exist.
fn load_vendor_template(name: &str) -> Result<crate::server::registry::ClientEntry> {
    let dirs = vendor_template_dirs();
    for dir in &dirs {
        let path = dir.join(format!("{name}.toml"));
        if let Ok(text) = std::fs::read_to_string(&path) {
            return toml::from_str(&text).with_context(|| format!("bad vendor template {}", path.display()));
        }
    }
    let mut have: Vec<String> = Vec::new();
    for dir in &dirs {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                if let Some(n) = e.file_name().to_str().and_then(|s| s.strip_suffix(".toml")) {
                    have.push(n.to_string());
                }
            }
        }
    }
    have.sort();
    have.dedup();
    let note = if have.is_empty() { format!("no templates found in any of: {}", dirs.iter().map(|d| d.display().to_string()).collect::<Vec<_>>().join(", ")) } else { format!("available templates: {}", have.join(", ")) };
    anyhow::bail!("no vendor template named {name}; {note}")
}

// ---------------------------------------------------------------------------
// Agent-side (client) dispatch helpers. These back the local queue
// subcommands and double as the SSH wire surface via --json.

use std::path::PathBuf;

fn agent_root(flag: &Option<String>) -> Result<PathBuf> {
    crate::agent::queue::resolve_root(flag.as_deref())
}

fn jprint(v: impl serde::Serialize) {
    println!("{}", serde_json::to_string(&v).unwrap_or_default());
}

/// json! object -> field map, for building wire responses.
fn jfields(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    v.as_object().cloned().unwrap_or_default()
}

fn jobj(fields: serde_json::Map<String, serde_json::Value>) -> serde_json::Value {
    let mut o = serde_json::Map::new();
    o.insert("protocol".into(), serde_json::json!(crate::proto::wire::PROTOCOL_VERSION));
    o.insert("ok".into(), serde_json::json!(true));
    o.extend(fields);
    serde_json::Value::Object(o)
}

/// Emit the wire error object and exit nonzero, or propagate the
/// error for the human-facing path.
fn jfail(json: bool, msg: &str) -> Result<()> {
    if json {
        jprint(serde_json::json!({"protocol": crate::proto::wire::PROTOCOL_VERSION,
                                  "ok": false, "error": msg}));
        std::process::exit(1);
    }
    anyhow::bail!("{msg}")
}

fn agent_enqueue(a: &EnqueueArgs) -> Result<()> {
    use std::io::Read;
    let root = agent_root(&a.root)?;
    let inner = || -> Result<String> {
        let body = if a.file == "-" {
            let mut b = String::new();
            std::io::stdin().read_to_string(&mut b)?;
            b
        } else {
            std::fs::read_to_string(&a.file).with_context(|| format!("no such file: {}", a.file))?
        };
        let raw_slug = match &a.slug {
            Some(s) => s.clone(),
            None if a.file == "-" => anyhow::bail!("slug required for stdin form"),
            None => std::path::Path::new(&a.file).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
        };
        let slug = crate::proto::task::sanitize_slug(&raw_slug);
        if slug.is_empty() {
            anyhow::bail!("empty slug after sanitization");
        }
        crate::agent::queue::enqueue(&root, &slug, &body)
    };
    match inner() {
        Ok(name) => {
            if a.json {
                jprint(jobj(jfields(serde_json::json!({"name": name}))));
            } else {
                println!("enqueued: {name}");
                if crate::agent::daemon::alive(&root)?.is_none() {
                    eprintln!("note: queue daemon not running; start with `shunt start`");
                }
            }
            Ok(())
        }
        Err(e) => jfail(a.json, &format!("{e:#}")),
    }
}

fn agent_new(a: &NewArgs) -> Result<()> {
    let json = a.json;
    let inner = || -> Result<String> {
        if a.cmd.is_empty() {
            anyhow::bail!("missing command after --");
        }
        let meta = crate::proto::task::TaskMeta { slug: crate::proto::task::sanitize_slug(&a.slug), guard: a.guard.clone(), workdir: None, env: Default::default(), cmd: a.cmd.clone() };
        if meta.slug.is_empty() {
            anyhow::bail!("empty slug after sanitization");
        }
        let body = crate::agent::exec::render_task(&meta);
        let root = agent_root(&a.root)?;
        crate::agent::queue::enqueue(&root, &meta.slug, &body)
    };
    match inner() {
        Ok(name) => {
            if json {
                jprint(jobj(jfields(serde_json::json!({"name": name}))));
            } else {
                println!("enqueued: {name}");
                let root = agent_root(&a.root)?;
                if crate::agent::daemon::alive(&root)?.is_none() {
                    eprintln!("note: queue daemon not running; start with `shunt start`");
                }
            }
            Ok(())
        }
        Err(e) => jfail(json, &format!("{e:#}")),
    }
}

fn agent_cancel(a: &PatternArgs) -> Result<()> {
    let root = agent_root(&a.root)?;
    let m = crate::agent::queue::match_pending(&root, &a.pattern)?;
    match m.len() {
        0 => jfail(a.json, &format!("no pending task matches '{}'", a.pattern)),
        1 => {
            crate::agent::queue::remove_pending(&root, &m[0])?;
            if a.json {
                jprint(jobj(jfields(serde_json::json!({"name": m[0]}))));
            } else {
                println!("cancelled: {}", m[0]);
            }
            Ok(())
        }
        _ => jfail(a.json, &format!("pattern matches several tasks, refusing:\n  {}", m.join("\n  "))),
    }
}

fn agent_prio(a: &PrioArgs) -> Result<()> {
    let root = agent_root(&a.root)?;
    let m = crate::agent::queue::match_pending(&root, &a.pattern)?;
    if m.len() != 1 {
        return jfail(a.json, &format!("need exactly one pending match, got {}:\n  {}", m.len(), m.join("\n  ")));
    }
    let base = &m[0];
    let rest = base.split_once('_').map(|(_, r)| r).unwrap_or(base);
    let new = format!("{:04}_{rest}", a.n);
    // Entry shapes: a dir renames to a dir; a legacy flat file keeps
    // its `.sh` suffix so it stays visible to the listing.
    let (from, to) = if crate::agent::queue::is_dir_entry(&root, "pending", base) { (root.join("pending").join(base), root.join("pending").join(&new)) } else { (root.join("pending").join(format!("{base}.sh")), root.join("pending").join(format!("{new}.sh"))) };
    // refuse to clobber an existing pending task (bash `mv -f` defect,
    // design.md §3.14); check BOTH shapes — a dir would silently
    // coexist with a legacy `NNNN_slug.sh` of the same name.
    let occupied = to.exists() || root.join("pending").join(format!("{new}.sh")).exists();
    if occupied && new != *base {
        return jfail(a.json, &format!("target name exists: {new}"));
    }
    std::fs::rename(&from, &to)?;
    if a.json {
        jprint(jobj(jfields(serde_json::json!({"from": base, "to": new}))));
    } else {
        println!("renamed: {base} -> {new}");
    }
    Ok(())
}

fn agent_kill(a: &PatternArgs) -> Result<()> {
    let root = agent_root(&a.root)?;
    let m = crate::agent::queue::match_running(&root, &a.pattern)?;
    match m.len() {
        0 => jfail(a.json, &format!("no running task matches '{}'", a.pattern)),
        1 => match crate::agent::exec::kill_task(&root, &m[0]) {
            Ok((pgid, escalated)) => {
                if a.json {
                    jprint(jobj(jfields(serde_json::json!({"pgid": pgid, "escalated": escalated}))));
                } else {
                    println!("killing {} (pgid {pgid})", m[0]);
                    if escalated {
                        println!("still alive after 10 s; sent SIGKILL");
                    } else {
                        println!("terminated");
                    }
                }
                Ok(())
            }
            Err(e) => jfail(a.json, &format!("{e:#}")),
        },
        _ => jfail(a.json, &format!("pattern matches several tasks, refusing:\n  {}", m.join("\n  "))),
    }
}

fn agent_workers(a: &WorkersArgs) -> Result<()> {
    let root = agent_root(&a.root)?;
    let set_to = match (&a.n, &a.action) {
        (Some(n), _) => Some(*n),
        (None, Some(WorkerAction::Set { n })) => Some(*n),
        _ => None,
    };
    if let Some(n) = set_to {
        std::fs::write(root.join("workers"), format!("{n}\n"))?;
    }
    let w = crate::agent::daemon::resolve_workers(&root);
    if a.json {
        jprint(jobj(jfields(serde_json::json!({"workers": w.value, "source": w.source}))));
    } else {
        println!("workers: {} ({})", w.value, w.source);
    }
    Ok(())
}

fn agent_shutdown(a: &ShutdownArgs) -> Result<()> {
    let root = agent_root(&a.root)?;
    let counts = || -> serde_json::Value {
        serde_json::json!({
            "running": crate::agent::queue::running_list(&root).map(|v| v.len()).unwrap_or(0),
            "pending": crate::agent::queue::pending_list(&root).map(|v| v.len()).unwrap_or(0),
        })
    };
    match a.action {
        ShutdownAction::On { grace_s, ref cmd } => {
            if let Some(c) = cmd {
                crate::agent::shutdown::set_cmd(&root, c.as_str())?;
            }
            crate::agent::shutdown::arm(&root, grace_s)?;
            let st = crate::agent::shutdown::status(&root)?;
            if a.json {
                let mut f = serde_json::json!({"armed": true, "grace_s": st.grace_s, "cmd": st.cmd});
                f.as_object_mut().unwrap().extend(counts().as_object().unwrap().clone());
                jprint(jobj(jfields(f)));
            } else {
                println!("armed: shutdown_on_idle set; poweroff fires {}s after pending/ AND running/ are both empty (cmd: {})", st.grace_s, st.cmd);
            }
        }
        ShutdownAction::Off => {
            crate::agent::shutdown::disarm(&root)?;
            if a.json {
                let mut f = serde_json::json!({"armed": false});
                f.as_object_mut().unwrap().extend(counts().as_object().unwrap().clone());
                jprint(jobj(jfields(f)));
            } else {
                println!("disarmed: shutdown_on_idle removed (a running countdown cancels within 1 s)");
            }
        }
        ShutdownAction::Status => {
            let st = crate::agent::shutdown::status(&root)?;
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

fn agent_status(a: &StatusArgs) -> Result<()> {
    use crate::proto::wire::{DaemonState, ExitRecord};
    let root = agent_root(&a.root)?;
    let alive = crate::agent::daemon::alive(&root)?;
    let report = crate::proto::wire::StatusReport { protocol: crate::proto::wire::PROTOCOL_VERSION, daemon: DaemonState { alive: alive.is_some(), pid: alive }, workers: crate::agent::daemon::resolve_workers(&root), running: crate::agent::queue::running_list(&root)?, pending: crate::agent::queue::pending_list(&root)?, done_recent: crate::agent::queue::recent_exits(&root, "done", 5).into_iter().map(|(name, rc)| ExitRecord { name, rc }).collect(), failed_recent: crate::agent::queue::recent_exits(&root, "failed", 5).into_iter().map(|(name, rc)| ExitRecord { name, rc }).collect(), stop: root.join("STOP").exists(), shutdown: crate::agent::shutdown::status(&root)? };
    if a.json {
        jprint(&report);
        return Ok(());
    }
    println!("== daemon ==");
    match report.daemon.pid {
        Some(pid) if report.daemon.alive => println!("  running (pid {pid})"),
        _ => println!("  stopped   (start: shunt start)"),
    }
    println!("  workers: {} ({})", report.workers.value, report.workers.source);
    if report.stop {
        println!("  STOP sentinel present (draining)");
    }
    println!("== running ==");
    if report.running.is_empty() {
        println!("  (idle)");
    } else {
        for t in &report.running {
            println!("  {t}");
        }
    }
    println!("== pending (execution order) ==");
    if report.pending.is_empty() {
        println!("  (empty)");
    } else {
        for t in &report.pending {
            println!("  {t}");
        }
    }
    for (label, recs) in [("done", &report.done_recent), ("failed", &report.failed_recent)] {
        println!("== {label} (recent) ==");
        for r in recs {
            println!("  {:<48} rc={}", r.name, r.rc);
        }
    }
    println!("logs: tail -f {0}/running/<task>/log   (daemon: {0}/daemon.log)", root.display());
    Ok(())
}

/// Find task logs matching `pat` across running/, done/, failed/ —
/// entry dirs' `log` files plus any flat `*.log` leftovers (legacy
/// sidecars, `<clean>.interrupted.<ts>.log` sweep products).
fn find_logs(root: &std::path::Path, pat: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for track in ["running", "done", "failed"] {
        for name in crate::agent::queue::list_entries(root, track).unwrap_or_default() {
            if name.contains(pat) {
                let log = crate::agent::queue::piece_file(root, track, &name, "log");
                if log.exists() {
                    out.push(log);
                }
            }
        }
        if let Ok(rd) = std::fs::read_dir(root.join(track)) {
            for ent in rd.flatten() {
                let name = ent.file_name().to_string_lossy().into_owned();
                if ent.file_type().map(|t| t.is_file()).unwrap_or(false) && name.ends_with(".log") && name.contains(pat) {
                    out.push(ent.path());
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn print_file(path: &std::path::Path) {
    use std::io::Write;
    match std::fs::read(path) {
        Ok(bytes) => {
            let _ = std::io::stdout().write_all(&bytes);
        }
        Err(e) => eprintln!("shunt: cannot read {}: {e}", path.display()),
    }
}

fn follow_file(path: &std::path::Path) -> Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::End(0))?;
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let n = f.read_to_end(&mut buf)?;
        if n > 0 {
            use std::io::Write;
            let _ = std::io::stdout().write_all(&buf);
            let _ = std::io::stdout().flush();
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

fn agent_logs(a: &LogsArgs) -> Result<()> {
    let root = agent_root(&a.root)?;
    match &a.pattern {
        None => agent_dlog(&RootArgs { root: a.root.clone(), json: false }),
        Some(pat) => {
            let m = find_logs(&root, pat);
            match m.len() {
                0 => anyhow::bail!("no task log matches '{pat}'"),
                1 => {
                    print_file(&m[0]);
                    if a.follow {
                        follow_file(&m[0])?;
                    }
                    Ok(())
                }
                _ => {
                    eprintln!("pattern matches several logs, refusing:");
                    for p in &m {
                        eprintln!("  {}", p.display());
                    }
                    anyhow::bail!("ambiguous log pattern '{pat}'")
                }
            }
        }
    }
}

fn agent_dlog(a: &RootArgs) -> Result<()> {
    let root = agent_root(&a.root)?;
    let path = root.join("daemon.log");
    // Render JSONL human-readable: `[ts] event k=v`.
    if let Ok(text) = std::fs::read_to_string(&path) {
        for line in text.lines() {
            match serde_json::from_str::<serde_json::Value>(line) {
                Ok(v) if v.is_object() => {
                    let ts = v.get("ts").and_then(|t| t.as_str()).unwrap_or("");
                    let ev = v.get("event").and_then(|t| t.as_str()).unwrap_or("");
                    let rest: Vec<String> = v.as_object().map(|o| o.iter().filter(|(k, _)| k.as_str() != "ts" && k.as_str() != "event").map(|(k, v)| format!("{k}={v}")).collect()).unwrap_or_default();
                    println!("[{ts}] {ev} {}", rest.join(" "));
                }
                _ => println!("{line}"),
            }
        }
    }
    Ok(())
}
