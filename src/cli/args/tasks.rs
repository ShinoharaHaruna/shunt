//! Task/queue operation args (mostly client-side, §10).

use clap::{Args, Subcommand};

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
    /// Reschedule budget: on rc 75 ("resource busy") — or a
    /// retry-declared OOM heuristic — the daemon requeues the task
    /// up to N extra times instead of failing it
    #[arg(long)]
    pub retry: Option<u32>,
    /// Seconds a rescheduled task waits before it is claimable
    /// again (default 60)
    #[arg(long = "retry-delay")]
    pub retry_delay_s: Option<u64>,
    /// Declared resource estimates (MiB / cores) — recorded in the
    /// task head and shown beside measured usage; never gated on
    #[arg(long = "gpu-mem-mb")]
    pub gpu_mem_mb: Option<u32>,
    #[arg(long = "ram-mb")]
    pub ram_mb: Option<u32>,
    #[arg(long = "cpu-cores")]
    pub cpu_cores: Option<u32>,
    /// KEY=value env overlay for this task (repeatable); beats the
    /// project binding's env per key — e.g.
    /// --env CUDA_VISIBLE_DEVICES=1 pins GPU 1
    #[arg(long = "env", value_name = "KEY=value")]
    pub env: Vec<String>,
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
    /// Named queue on the selected server client
    #[arg(long)]
    pub queue: Option<String>,
    /// Stage held: the dispatcher skips this task on every sweep
    /// until `shunt spool release <id>`. Optionally record why
    /// (`--hold=REASON`; `=` is required so the reason can't be
    /// mistaken for a file argument)
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "", value_name = "REASON")]
    pub hold: Option<String>,
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
    #[arg(long, conflicts_with = "all")]
    pub root: Option<String>,
    /// Identity for the rooms block (server aggregate only, §17.5):
    /// unread counts are computed for this label; default `admin`
    /// (the operator); case-canonicalized to a registered project.
    /// Implies the aggregate — rooms are server-local, so a -c wire
    /// op or a local --root has none to report.
    #[arg(long = "as", conflicts_with_all = ["client", "root"])]
    pub as_label: Option<String>,
    /// Emit a single JSON object (wire protocol)
    #[arg(long)]
    pub json: bool,
    /// Named queue on the selected server client
    #[arg(long, conflicts_with_all = ["root", "all", "as_label"])]
    pub queue: Option<String>,
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
    /// Named queue on the selected server client
    #[arg(long, conflicts_with = "root")]
    pub queue: Option<String>,
}

#[derive(Args)]
pub struct RootArgs {
    /// Queue root; falls back to $SHUNT_ROOT
    // visible `--qroot`: spec §5's wire table spells the flag after
    // the registry row's field name; accepting it costs nothing.
    #[arg(long, visible_alias = "qroot")]
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
    /// Slug as an option — the §5 wire form `enqueue - --slug x`.
    /// Merged with the positional; specifying both is an error.
    #[arg(long = "slug", conflicts_with = "slug")]
    pub slug_opt: Option<String>,
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
    /// Reschedule budget: requeue on rc 75 up to N extra times
    #[arg(long)]
    pub retry: Option<u32>,
    /// Seconds a rescheduled task waits before it is claimable
    /// again (default 60)
    #[arg(long = "retry-delay")]
    pub retry_delay_s: Option<u64>,
    /// Declared resource estimates — annotation only, never gated
    #[arg(long = "gpu-mem-mb")]
    pub gpu_mem_mb: Option<u32>,
    #[arg(long = "ram-mb")]
    pub ram_mb: Option<u32>,
    #[arg(long = "cpu-cores")]
    pub cpu_cores: Option<u32>,
    /// KEY=value env overlay (repeatable), e.g. --env CUDA_VISIBLE_DEVICES=1
    #[arg(long = "env", value_name = "KEY=value")]
    pub env: Vec<String>,
    #[arg(long)]
    pub root: Option<String>,
    #[arg(long)]
    pub json: bool,
    /// Command to wrap, after `--`
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub cmd: Vec<String>,
}

impl NewArgs {
    /// `new`/`mktask` share the meta build: argv → validated
    /// `TaskMeta` (id/workdir are the server spool's fields, so they
    /// stay `None` here — the box-side allocator never dedups a
    /// locally-generated task). The slug refusal names the RAW flag
    /// so the user sees what they typed, not the sanitized form.
    /// `timeout`/`retry` go in unfiltered: render_task's own `> 0`
    /// check drops a literal `0`, so this path never needed the
    /// submit-side normalization.
    pub(crate) fn task_meta(&self) -> anyhow::Result<crate::proto::task::TaskMeta> {
        if self.cmd.is_empty() {
            anyhow::bail!("missing command after --");
        }
        let meta = crate::proto::task::TaskMeta { id: None, slug: crate::proto::task::sanitize_slug(&self.slug), guard: self.guard.clone(), workdir: None, env: crate::proto::task::parse_env_pairs(&self.env)?, cmd: self.cmd.clone(), timeout_s: self.timeout, retry: self.retry, retry_delay_s: self.retry_delay_s, gpu_mem_mb: self.gpu_mem_mb, ram_mb: self.ram_mb, cpu_cores: self.cpu_cores };
        if let Some(p) = crate::proto::task::slug_problem(&meta.slug) {
            anyhow::bail!("bad slug '{}': {p}", self.slug);
        }
        crate::proto::task::validate_meta(&meta)?;
        Ok(meta)
    }
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
    /// Named queue on the selected server client
    #[arg(long, conflicts_with = "root")]
    pub queue: Option<String>,
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
    /// Named queue on the selected server client
    #[arg(long, conflicts_with = "root")]
    pub queue: Option<String>,
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
    /// Named queue on the selected server client
    #[arg(long, global = true, conflicts_with = "root")]
    pub queue: Option<String>,
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
    /// Named queue on the selected server client
    #[arg(long, global = true, conflicts_with = "root")]
    pub queue: Option<String>,
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
