//! clap command tree: `Cli`, `Command`, and every Args/Action
//! type (design.md §10). Dispatch lives in `mod.rs::run`.

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
    #[command(arg_required_else_help = true)]
    Submit(SubmitArgs),
    /// Queue status: -c one client, --all server aggregate,
    /// --root/$SHUNT_ROOT local queue; bare prints this help
    Status(StatusArgs),
    /// Task or daemon logs
    Logs(LogsArgs),
    /// Daemon log only (client convenience)
    Dlog(RootArgs),
    /// Manage the server-side staging queue
    #[command(arg_required_else_help = true)]
    Spool(SpoolCmd),
    /// Task ledger: what a spool id became on the box and how it
    /// ended (design.md §4.5)
    #[command(arg_required_else_help = true)]
    Task(TaskCmd),
    /// Manage registered experiment boxes
    #[command(arg_required_else_help = true)]
    Client(ClientCmd),
    /// Manage registered projects and their client bindings
    #[command(arg_required_else_help = true)]
    Project(ProjectCmd),
    /// Run the spool dispatcher loop (server)
    Dispatch(DispatchArgs),
    /// Box-side SOCKS5 tunnel over ssh -R (server)
    #[command(arg_required_else_help = true)]
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
    #[command(arg_required_else_help = true)]
    Enqueue(EnqueueArgs),
    /// Wrap argv as a guarded task and enqueue it (client)
    #[command(arg_required_else_help = true)]
    New(NewArgs),
    /// Print the generated task body to stdout (client)
    #[command(arg_required_else_help = true)]
    Mktask(NewArgs),
    /// Remove a pending task by name/pattern
    #[command(arg_required_else_help = true)]
    Cancel(PatternArgs),
    /// Renumber a pending task into the priority band 0000-0099
    #[command(arg_required_else_help = true)]
    Prio(PrioArgs),
    /// Kill a running task's process group
    #[command(arg_required_else_help = true)]
    Kill(PatternArgs),
    /// Get or set the worker pool size
    Workers(WorkersArgs),
    /// Idle-shutdown control (client) or remote arm via -c (server)
    #[command(arg_required_else_help = true)]
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
    /// (default: server.toml `default_client`)
    #[arg(short, long)]
    pub client: Option<String>,
    /// Guard path: task exits 0 early if this exists at run time
    #[arg(long)]
    pub guard: Option<String>,
    /// Slug override (default: derived from cmd or file)
    #[arg(long)]
    pub slug: Option<String>,
    /// Existing task file to spool instead of wrapping argv
    #[arg(long, conflicts_with_all = ["cmd", "file_pos"])]
    pub file: Option<String>,
    /// Wall-clock budget in seconds; 0/omitted = unlimited
    #[arg(long)]
    pub timeout: Option<u64>,
    /// Task file to spool (same as --file): `submit -c x file.sh`.
    /// A bare word that is not a file is an error, not a command —
    /// commands must come after `--`, or a typo'd path would spool
    /// a task that execs a nonexistent remote script.
    #[arg(conflicts_with = "cmd")]
    pub file_pos: Option<String>,
    /// Command to wrap and spool, after `--`
    // `last` keeps `submit -c x -- echo hi` unambiguous next to the
    // file positional; `trailing_var_arg` cannot combine with `last`.
    #[arg(last = true, num_args = 1.., allow_hyphen_values = true)]
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
    /// Follow the log (client side only; refused over the wire)
    #[arg(short, long)]
    pub follow: bool,
    /// Print only the last N lines
    #[arg(short = 'n', long)]
    pub tail: Option<usize>,
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
    /// Wall-clock budget in seconds; 0/omitted = unlimited
    #[arg(long)]
    pub timeout: Option<u64>,
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
    /// New sequence number; must be inside the 0000-0099 priority
    /// band (renumbering into the normal sequence would corrupt it)
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
    // global: spec §10 order is `workers set 5 -c x` — a non-global
    // flag on the parent rejects args placed after the subcommand.
    #[arg(short, long, global = true)]
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
    // global: spec order is `shutdown on -c x` (§10 / arm path §4).
    #[arg(short, long, global = true)]
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
    /// Client name (default: server.toml `default_client`)
    // global: spec §10 documents `tunnel up|down|status -c client`.
    #[arg(short, long, global = true)]
    pub client: Option<String>,
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
    #[command(visible_alias = "rm")]
    Remove { task_id: String },
}

#[derive(Args)]
pub struct TaskCmd {
    #[command(subcommand)]
    pub action: TaskAction,
}

#[derive(Subcommand)]
pub enum TaskAction {
    /// List recorded tasks (pushed_at order)
    Ls {
        /// Emit one JSON object per line
        #[arg(long)]
        json: bool,
    },
    /// Show one task by spool id; when no outcome is recorded yet,
    /// queries the client's live status and writes a terminal
    /// result back to the ledger + events.jsonl
    Show {
        /// Task id as printed by `submit` (unique prefix works)
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Print the server-side event stream (events.jsonl)
    Events,
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
    /// Update a row's ssh endpoint after the vendor moved the box
    /// (rotating IP/port, rebuilt container). Also drops stale
    /// known_hosts entries, kills the mux master, clears the
    /// conn-fail streak, and tears down a recorded tunnel — things
    /// hand-editing the row would leave pinned to the dead endpoint.
    /// Warns when another row already sits at the same host:port
    /// (legal behind NAT, suspicious otherwise). Other row fields
    /// (qroot, deploy_root, expire, env_setup) are not updatable
    /// here — edit clients/<name>.toml directly; a moved qroot
    /// means relocating the remote queue, not just a string.
    Update {
        name: String,
        /// New SSH host
        #[arg(long)]
        host: Option<String>,
        /// New SSH port
        #[arg(long)]
        port: Option<u16>,
        /// New SSH user
        #[arg(long)]
        user: Option<String>,
        /// New SSH password (empty string clears)
        #[arg(long)]
        password: Option<String>,
        /// Mark the vendor as one that reassigns IP/port on restart
        #[arg(long)]
        endpoint_rotates: bool,
    },
    /// Deregister a box: remote stop + key revocation (best-effort),
    /// then row/keys/mux/state/spool/binding removal locally. On the
    /// reserved `localhost` this resets the row to factory defaults
    /// (it regenerates on next use — disabled=true is the opt-out).
    #[command(visible_alias = "rm")]
    Remove {
        name: String,
        /// Also `rm -rf <qroot>` on the box (queue, env, binary).
        /// Bind workdirs are reported, never deleted.
        #[arg(long)]
        purge: bool,
    },
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
    /// Run an ad-hoc command on the box through the managed ssh
    /// (mux + retries + our key/known_hosts — no flag soup)
    Exec {
        name: String,
        /// Command + args, e.g. `-- ls -la /root`
        #[arg(trailing_var_arg = true, required = true)]
        cmd: Vec<String>,
    },
}

#[derive(Args)]
pub struct ProjectCmd {
    #[command(subcommand)]
    pub action: ProjectAction,
}

#[derive(Subcommand)]
pub enum ProjectAction {
    /// Register a project
    Add {
        name: String,
        /// Canonical checkout on the dev machine (path or git URL)
        #[arg(long)]
        repo: Option<String>,
        /// One-line description — helps agents pick the right project
        #[arg(long)]
        desc: Option<String>,
    },
    /// List registered projects
    Ls,
    /// Show one project row
    Show { name: String },
    /// Remove a project row
    #[command(visible_alias = "rm")]
    Remove { name: String },
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

#[cfg(test)]
mod tests {
    //! Pin `global = true` on `-c` for the subcommand-grouped Args
    //! structs: spec §10 documents the flag after the action
    //! (`tunnel up -c x`, `shutdown on -c x`, `workers set 5 -c x`).
    //! Global flags must keep parsing in the pre-action position too.

    use super::*;

    fn parse(argv: &[&str]) -> Cli {
        Cli::try_parse_from(argv).unwrap_or_else(|e| panic!("{argv:?} failed: {e}"))
    }

    #[test]
    fn tunnel_client_after_subcommand() {
        let cli = parse(&["shunt", "tunnel", "up", "-c", "box1"]);
        let Command::Tunnel(t) = cli.command else { panic!() };
        assert_eq!(t.client.as_deref(), Some("box1"));
        assert!(matches!(t.action, TunnelAction::Up { port: None }));
    }

    #[test]
    fn tunnel_client_before_subcommand_still_works() {
        let cli = parse(&["shunt", "tunnel", "-c", "box1", "status"]);
        let Command::Tunnel(t) = cli.command else { panic!() };
        assert_eq!(t.client.as_deref(), Some("box1"));
        assert!(matches!(t.action, TunnelAction::Status));
    }

    #[test]
    fn shutdown_client_after_subcommand() {
        let cli = parse(&["shunt", "shutdown", "on", "-c", "box1", "--json"]);
        let Command::Shutdown(s) = cli.command else { panic!() };
        assert_eq!(s.client.as_deref(), Some("box1"));
        assert!(s.json);
        assert!(matches!(s.action, ShutdownAction::On { grace_s: None, .. }));
    }

    #[test]
    fn shutdown_client_before_subcommand_still_works() {
        let cli = parse(&["shunt", "shutdown", "-c", "box1", "status"]);
        let Command::Shutdown(s) = cli.command else { panic!() };
        assert_eq!(s.client.as_deref(), Some("box1"));
        assert!(matches!(s.action, ShutdownAction::Status));
    }

    #[test]
    fn workers_client_after_subcommand() {
        let cli = parse(&["shunt", "workers", "set", "5", "-c", "box1"]);
        let Command::Workers(w) = cli.command else { panic!() };
        assert_eq!(w.client.as_deref(), Some("box1"));
        assert!(matches!(w.action, Some(WorkerAction::Set { n: 5 })));
    }

    #[test]
    fn workers_get_client_after_subcommand() {
        let cli = parse(&["shunt", "workers", "get", "-c", "box1"]);
        let Command::Workers(w) = cli.command else { panic!() };
        assert_eq!(w.client.as_deref(), Some("box1"));
        assert!(matches!(w.action, Some(WorkerAction::Get)));
    }
}
