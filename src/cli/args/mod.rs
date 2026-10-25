//! clap command tree: `Cli`, `Command`, and every Args/Action
//! type (design.md §10). Dispatch lives in `cli/mod.rs::run`.
//! Definitions are grouped by side: `tasks` (queue/task ops,
//! mostly client-side), `server` (registry/spool/dispatch/
//! tunnel admin), `room` (§17 surface), `misc` (version,
//! upgrade, completions, probe, feedback).

use clap::{Parser, Subcommand};

mod misc;
mod room;
mod server;
mod tasks;
#[cfg(test)]
mod tests;

pub use misc::{CompletionsArgs, FeedbackAction, FeedbackArgs, ProbeArgs, UpgradeArgs, VersionArgs};
pub use room::{RoomAction, RoomCmd};
pub use server::{ClientAction, ClientCmd, DispatchArgs, ProjectAction, ProjectCmd, QueueAction, QueueCmd, ServeAction, ServeArgs, SpoolAction, SpoolCmd, TaskAction, TaskCmd, TunnelAction, TunnelArgs};
pub use tasks::{CatArgs, CleanArgs, EnqueueArgs, LogsArgs, NewArgs, PatternArgs, PrioArgs, RootArgs, ShutdownAction, ShutdownArgs, StatusArgs, SubmitArgs, WorkerAction, WorkersArgs};

#[derive(Parser)]
#[command(
    name = "shunt",
    version = env!("SHUNT_VERSION"),
    about = "Experiment queues, machine management, and agent coordination over SSH or locally",
    // Single-line long_about: clap wraps help text to $COLUMNS at
    // render time; baked \n would render the same breaks at every
    // terminal width and fight clap's own wrap.
    long_about = "shunt manages experiments on local workstations, lab servers, and cloud CPU or GPU machines. One binary runs on both sides: server commands manage machines, project bindings, staged tasks, and coordination rooms; each machine runs independent named queues with concurrent workers, task logs, and resource telemetry. Use the same CLI locally or over SSH, with --json on status and task queries for agents and scripts. Idle shutdown and billing metadata are optional."
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
    /// Print a queued task's script (task.sh) verbatim
    #[command(arg_required_else_help = true)]
    Cat(CatArgs),
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
    /// Manage named queues on a registered client
    #[command(arg_required_else_help = true)]
    Queue(QueueCmd),
    /// Run the spool dispatcher loop (server)
    Dispatch(DispatchArgs),
    /// Serve the read-only ops dashboard: embedded web UI plus
    /// websocket snapshots of clients, spool, tasks, and rooms
    /// (design.md §18)
    Serve(ServeArgs),
    /// Box-side SOCKS5 tunnel over ssh -R (server)
    #[command(arg_required_else_help = true)]
    Tunnel(TunnelArgs),
    /// Coordination rooms for projects sharing compute —
    /// server-local group chats (design.md §17)
    #[command(arg_required_else_help = true)]
    Room(RoomCmd),

    /// Create queue dirs under --root (client)
    Init(RootArgs),
    /// Run the queue daemon in the foreground (client)
    Daemon(RootArgs),
    /// Start the daemon in the background (client)
    Start(RootArgs),
    /// Stop the daemon (client)
    Stop(RootArgs),
    /// Handoff-restart the daemon: SIGUSR2 re-execs the current image
    /// in place, adopting in-flight tasks — the §3.5a upgrade path
    /// (client)
    Restart(RootArgs),
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
    /// Bulk-remove queue entries by set (design.md §3.9b);
    /// bare invocation refuses with per-track counts
    Clean(CleanArgs),
    /// Get or set the worker pool size
    Workers(WorkersArgs),
    /// Idle-shutdown control (client) or remote arm via -c (server)
    #[command(arg_required_else_help = true)]
    Shutdown(ShutdownArgs),
    /// Print machine facts as JSON (client)
    Probe(ProbeArgs),
    /// Print a shell completion script to stdout
    Completions(CompletionsArgs),
    /// Print version + build info; -c queries the binary on a box
    /// (forwards the --version flag, which every remote has)
    Version(VersionArgs),
    /// Self-upgrade to the latest GitHub release; -c upgrades the
    /// box's binary to the latest musl release instead (§16)
    Upgrade(UpgradeArgs),
    /// File a report as a GitHub issue on the shunt repo
    /// (design.md §14); `ls`/`retry` manage the local ledger
    #[command(arg_required_else_help = true)]
    Feedback(FeedbackArgs),
}
