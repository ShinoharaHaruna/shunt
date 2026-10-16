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
    long_about = "shunt runs on both sides. Server subcommands (submit, client, project,\nspool, dispatch) operate the registry and staging queue on the dev\nmachine. Client subcommands (daemon, enqueue, status, ...) operate\nthe local queue and double as the SSH wire protocol via --json."
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
    /// Forward to a client over SSH (server side)
    #[arg(short, long)]
    pub client: Option<String>,
    #[arg(long)]
    pub root: Option<String>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct ShutdownArgs {
    /// on [grace_s] | off | status
    #[command(subcommand)]
    pub action: ShutdownAction,
    /// Forward to a client over SSH (server side)
    #[arg(short, long)]
    pub client: Option<String>,
    #[arg(long)]
    pub root: Option<String>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Subcommand)]
pub enum ShutdownAction {
    /// Arm idle poweroff
    On {
        /// Countdown seconds (default 60)
        grace_s: Option<u64>,
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

#[derive(Subcommand)]
pub enum ClientAction {
    /// Register a box (server-local, zero network traffic)
    Add {
        /// Registry name
        name: String,
        /// Vendor template under etc/vendors/ (e.g. autodl)
        #[arg(long)]
        template: Option<String>,
        /// SSH host
        #[arg(long)]
        host: Option<String>,
        /// SSH port
        #[arg(long)]
        port: Option<u16>,
        /// SSH user (template default: root)
        #[arg(long)]
        user: Option<String>,
        /// SSH private key path
        #[arg(long)]
        key: Option<String>,
        /// Queue dir on the box (template default: deploy_root/queue)
        #[arg(long)]
        qroot: Option<String>,
        /// Vendor work root (template default for autodl: /root/autodl-tmp)
        #[arg(long)]
        deploy_root: Option<String>,
    },
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

fn stub(what: String) -> Result<()> {
    eprintln!("shunt: {what}");
    anyhow::bail!("not yet implemented")
}

pub fn run(cli: Cli) -> Result<()> {
    use crate::server::registry;
    match cli.command {
        Command::Submit(a) => stub(format!(
            "submit: project={:?} client={} guard={:?} slug={:?} file={:?} cmd={:?}",
            a.project, a.client, a.guard, a.slug, a.file, a.cmd
        )),
        Command::Status(a) => stub(format!(
            "status: all={} client={:?} root={:?} json={}",
            a.all, a.client, a.root, a.json
        )),
        Command::Logs(a) => stub(format!(
            "logs: pattern={:?} client={:?} root={:?} follow={}",
            a.pattern, a.client, a.root, a.follow
        )),
        Command::Dlog(a) => stub(format!("dlog: root={:?} json={}", a.root, a.json)),
        Command::Dispatch(a) => crate::server::dispatch::run(a.once),

        Command::Spool(c) => match c.action {
            SpoolAction::Ls => stub("spool ls".into()),
            SpoolAction::Rm { task_id } => stub(format!("spool rm {task_id}")),
        },

        Command::Client(c) => match c.action {
            ClientAction::Add {
                name,
                template,
                host,
                port,
                user,
                key,
                qroot,
                deploy_root,
            } => client_add(
                &name,
                template.as_deref(),
                ClientAddOpts {
                    host,
                    port,
                    user,
                    key,
                    qroot,
                    deploy_root,
                },
            ),
            ClientAction::Ls => {
                for n in registry::list_names("clients")? {
                    println!("{n}");
                }
                Ok(())
            }
            ClientAction::Show { name } => {
                let e = registry::load_client(&name)?;
                println!("{}", toml::to_string_pretty(&e)?);
                Ok(())
            }
            ClientAction::Rm { name } => registry::remove("clients", &name),
            ClientAction::Probe { name } => stub(format!("client probe {name}")),
            ClientAction::Bootstrap { name } => stub(format!("client bootstrap {name}")),
        },

        Command::Project(c) => match c.action {
            ProjectAction::Add { name } => {
                let e = registry::ProjectEntry {
                    name: name.clone(),
                    bind: Vec::new(),
                };
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
            ProjectAction::Bind {
                name,
                client,
                workdir,
                env,
            } => {
                let mut e = registry::load_project(&name)?;
                let mut map = std::collections::BTreeMap::new();
                for kv in &env {
                    let (k, v) = kv
                        .split_once('=')
                        .ok_or_else(|| anyhow::anyhow!("--env expects KEY=value, got {kv}"))?;
                    map.insert(k.to_string(), v.to_string());
                }
                e.bind.retain(|b| b.client != client);
                e.bind.push(registry::Binding {
                    client,
                    workdir,
                    env: map,
                });
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

        Command::Init(a) => stub(format!("init: root={:?} json={}", a.root, a.json)),
        Command::Daemon(a) => stub(format!("daemon: root={:?} json={}", a.root, a.json)),
        Command::Start(a) => stub(format!("start: root={:?} json={}", a.root, a.json)),
        Command::Stop(a) => stub(format!("stop: root={:?} json={}", a.root, a.json)),
        Command::Enqueue(a) => stub(format!(
            "enqueue: file={} slug={:?} root={:?} json={}",
            a.file, a.slug, a.root, a.json
        )),
        Command::New(a) => stub(format!(
            "new: slug={} guard={:?} cmd={:?} root={:?} json={}",
            a.slug, a.guard, a.cmd, a.root, a.json
        )),
        Command::Mktask(a) => {
            // mktask can render for real: it is pure output.
            let meta = crate::proto::task::TaskMeta {
                slug: crate::proto::task::sanitize_slug(&a.slug),
                guard: a.guard,
                workdir: None,
                env: Default::default(),
                cmd: a.cmd,
            };
            if meta.slug.is_empty() {
                anyhow::bail!("empty slug after sanitization");
            }
            print!("{}", crate::agent::exec::render_task(&meta));
            Ok(())
        }
        Command::Cancel(a) => stub(format!(
            "cancel: pattern={} client={:?} root={:?} json={}",
            a.pattern, a.client, a.root, a.json
        )),
        Command::Prio(a) => stub(format!(
            "prio: pattern={} n={} client={:?} root={:?} json={}",
            a.pattern, a.n, a.client, a.root, a.json
        )),
        Command::Kill(a) => stub(format!(
            "kill: pattern={} client={:?} root={:?} json={}",
            a.pattern, a.client, a.root, a.json
        )),
        Command::Workers(a) => stub(format!(
            "workers: n={:?} client={:?} root={:?} json={}",
            a.n, a.client, a.root, a.json
        )),
        Command::Shutdown(a) => {
            let act = match a.action {
                ShutdownAction::On { grace_s } => format!("on grace={grace_s:?}"),
                ShutdownAction::Off => "off".into(),
                ShutdownAction::Status => "status".into(),
            };
            stub(format!(
                "shutdown: {act} client={:?} root={:?} json={}",
                a.client, a.root, a.json
            ))
        }
        Command::Probe(a) => stub(format!(
            "probe: deploy_root={} root={:?} json={}",
            a.deploy_root, a.root, a.json
        )),
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
}

fn client_add(name: &str, template: Option<&str>, opts: ClientAddOpts) -> Result<()> {
    use crate::server::registry::{ClientEntry, SshEndpoint};

    let mut entry = match template {
        Some(t) => load_vendor_template(t)?,
        None => ClientEntry {
            name: name.into(),
            ssh: SshEndpoint {
                host: String::new(),
                port: 22,
                user: "root".into(),
                key: String::new(),
            },
            qroot: String::new(),
            deploy_root: String::new(),
            facts: Default::default(),
            billing: None,
        },
    };
    entry.name = name.into();
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
    let p = crate::server::registry::save_client(&entry)?;
    println!("wrote {}", p.display());
    if entry.ssh.host.is_empty() || entry.ssh.key.is_empty() || entry.ssh.port == 0 {
        eprintln!("note: ssh fields still incomplete; edit the row before bootstrap");
    }
    Ok(())
}

/// Load `etc/vendors/<name>.toml` as a client-row seed. Looked up next
/// to the binary's source tree; falls back to $SHUNT_VENDOR_DIR.
fn load_vendor_template(name: &str) -> Result<crate::server::registry::ClientEntry> {
    let candidates = [
        std::env::var("SHUNT_VENDOR_DIR")
            .ok()
            .map(std::path::PathBuf::from),
        Some(std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("etc/vendors")),
    ];
    for dir in candidates.into_iter().flatten() {
        let path = dir.join(format!("{name}.toml"));
        if let Ok(text) = std::fs::read_to_string(&path) {
            return toml::from_str(&text)
                .with_context(|| format!("bad vendor template {}", path.display()));
        }
    }
    anyhow::bail!("no vendor template named {name} (expected etc/vendors/{name}.toml)")
}
