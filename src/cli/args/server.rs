//! Server-side admin args: dispatch, tunnel, spool, task ledger,
//! and the client/project/queue registry commands (§10).

use clap::{Args, Subcommand};

#[derive(Args)]
pub struct DispatchArgs {
    /// Single sweep of spool/, then exit (for tests)
    #[arg(long)]
    pub once: bool,
}

#[derive(Args)]
pub struct ServeArgs {
    /// `run` in the foreground, `up` detached, `down` stops.
    /// Bare `shunt serve` prints this command's help.
    #[command(subcommand)]
    pub action: Option<ServeAction>,
}

#[derive(Subcommand)]
pub enum ServeAction {
    /// Foreground, blocking — Ctrl-C stops
    Run(ServeFlags),
    /// Detached background start: pidfile + serve.log under
    /// $SHUNT_HOME; refuse when one is already up
    Up(ServeFlags),
    /// Stop the background instance (SIGTERM, verifies the pidfile's
    /// cmdline before signaling)
    Down,
}

#[derive(Args, Clone)]
pub struct ServeFlags {
    /// Listen address (default 127.0.0.1 — reach a remote dev
    /// machine with `ssh -L PORT:127.0.0.1:PORT <dev>`)
    #[arg(long, default_value = "127.0.0.1")]
    pub bind: String,
    /// Listen port
    #[arg(long, default_value_t = 8747)]
    pub port: u16,
    /// Access token — REQUIRED when --bind is not loopback. Checked
    /// as ?token=, the x-shunt-token header, or Authorization: Bearer
    #[arg(long)]
    pub token: Option<String>,
    /// Wire-probe cadence in seconds for the clients section (one
    /// ssh exec per queue per tick); local state updates every 2s
    #[arg(long, default_value_t = 10)]
    pub interval_s: u64,
}

impl ServeFlags {
    pub fn into_opts(self) -> crate::server::web::ServeOpts {
        crate::server::web::ServeOpts { bind: self.bind, port: self.port, token: self.token, interval_s: self.interval_s }
    }
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
    /// Hold a staged entry: the dispatcher skips it until released.
    /// Task id or unique prefix; REASON is a free-form note
    Hold { task_id: String, reason: Option<String> },
    /// Release a held entry so the next dispatch sweep delivers it.
    /// Task id or unique prefix
    Release { task_id: String },
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
    /// Explicit local path for the binary bootstrap ships
    /// (§15); stored on the row so re-bootstraps reuse it
    #[arg(long)]
    pub bin_path: Option<String>,
    /// Box is shared with other people/workloads — server-side
    /// `shutdown on` then refuses to arm idle poweroff
    #[arg(long)]
    pub shared: bool,
    /// Write <qroot>/exit_on_idle at bootstrap: daemon exits once
    /// the queue drains instead of staying resident (revived on
    /// next dispatch)
    #[arg(long)]
    pub exit_on_idle: bool,
    /// Initial queue name (default: default)
    #[arg(long)]
    pub queue: Option<String>,
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
        /// Mark the vendor as one that reassigns IP/port on restart.
        /// Pairs with --no-endpoint-rotates so a mislabeled row can
        /// be cleared (§4.1 keeps the flag update-managed); last one
        /// wins via overrides_with.
        #[arg(long, overrides_with = "no_endpoint_rotates")]
        endpoint_rotates: bool,
        /// Clear the endpoint_rotates mark on the row
        #[arg(long, overrides_with = "endpoint_rotates")]
        no_endpoint_rotates: bool,
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
    /// Probe raw SSH, scp the binary, remote init + start. Binary
    /// source: --bin-path > row bin_path > current_exe; the staged
    /// candidate is smoke-classified ok/warn/error before install
    /// (design.md §15).
    Bootstrap {
        name: String,
        /// Ship this local binary instead of the running one
        /// (e.g. a musl cross-build for an old-glibc box)
        #[arg(long)]
        bin_path: Option<String>,
    },
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
        /// Client name (or use --client)
        #[arg(required_unless_present = "client", conflicts_with = "client")]
        client_pos: Option<String>,
        #[arg(long, required_unless_present = "client_pos")]
        client: Option<String>,
        /// Named queue; omitted resolves the client default
        #[arg(long)]
        queue: Option<String>,
        #[arg(long)]
        workdir: String,
        /// KEY=value env overlay entries (repeatable)
        #[arg(long = "env", value_name = "KEY=value")]
        env: Vec<String>,
    },
    /// Remove a binding
    Unbind {
        name: String,
        /// Client name (or use --client)
        #[arg(required_unless_present = "client", conflicts_with = "client")]
        client_pos: Option<String>,
        #[arg(long, required_unless_present = "client_pos")]
        client: Option<String>,
        /// Named queue; omitted resolves the client default
        #[arg(long)]
        queue: Option<String>,
    },
}

#[derive(Args)]
pub struct QueueCmd {
    #[command(subcommand)]
    pub action: QueueAction,
    /// Client name (default: server.toml default_client)
    #[arg(short, long, global = true)]
    pub client: Option<String>,
    #[arg(long, global = true)]
    pub json: bool,
}

#[derive(Subcommand)]
pub enum QueueAction {
    /// Add queue configuration without contacting the client
    Add {
        name: String,
        #[arg(long)]
        qroot: String,
        #[arg(long, default_value = "")]
        env_setup: String,
        #[arg(long, default_value = "bash")]
        shell: String,
        #[arg(long)]
        exit_on_idle: bool,
        #[arg(long)]
        workers: Option<u32>,
    },
    /// List all registered queues
    Ls,
    /// Show a queue's configuration
    Show { name: String },
    /// Set the execution default without contacting the client
    Default { name: String },
    /// Remove an unreferenced queue, optionally deleting its qroot
    Rm {
        name: String,
        #[arg(long)]
        purge: bool,
    },
    /// Install the binary and initialize/start one queue
    Bootstrap {
        name: String,
        #[arg(long)]
        bin_path: Option<String>,
    },
    /// Push one queue's environment
    Env { name: String },
    /// Start one queue's daemon
    Start { name: String },
    /// Drain and stop one queue's daemon
    Stop { name: String },
    /// Handoff-restart one queue's daemon (SIGUSR2 re-exec, §3.5a)
    Restart { name: String },
}
