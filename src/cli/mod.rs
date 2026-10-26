//! clap command tree and subcommand dispatch (design.md §10).
//! Server subcommands operate the registry/spool on the dev machine;
//! client subcommands are also the SSH wire surface (`--json`).
//! Server handlers dispatch over the ssh/local transports; client
//! handlers run the filesystem queue state machine (design.md §3).
//!
//! `run` below is a pure dispatch table: every Command variant's
//! handler lives under `cmd/` (one file per command group), beside
//! the pre-existing `queues`/`feedback`/`upgrade`/`server`/`local`
//! dispatch surfaces.

mod args;
mod cmd;
mod feedback;
mod local;
mod queues;
mod server;
mod upgrade;

use anyhow::Result;

pub use args::{Cli, Command};

/// Shared skeleton for dual-mode commands (`status`, `logs`,
/// `cancel`, `prio`, `kill`, `workers`, `shutdown`): `-c`/registry
/// default_client forwards the op over the wire; a local queue in
/// scope runs the local handler; neither prints subcommand help.
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
    if let Some(name) = server::resolve_client(client, root)? {
        return wire(&name);
    }
    anyhow::ensure!(queue.is_none(), "--queue requires a server client; use -c CLIENT (without --root/SHUNT_ROOT), or configure default_client");
    if server::rootless(root) {
        return server::print_subhelp(sub);
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
    match cli.command {
        Command::Submit(a) => cmd::submit(&a),
        Command::Status(a) => cmd::status(&a),
        Command::Logs(a) => cmd::logs(&a),
        Command::Cat(a) => cmd::cat(&a),
        Command::Dlog(a) => cmd::dlog(&a),
        Command::Spool(c) => cmd::spool(&c),
        Command::Task(c) => cmd::task(&c),
        Command::Client(c) => cmd::client(&c),
        Command::Project(c) => cmd::project(&c),
        Command::Queue(c) => queues::cmd_queue(&c),
        Command::Dispatch(a) => cmd::dispatch(&a),
        Command::Serve(a) => cmd::serve(&a),
        Command::Tunnel(a) => cmd::tunnel(&a),
        Command::Room(c) => server::cmd_room(&c),
        Command::Init(a) => cmd::init(&a),
        Command::Daemon(a) => cmd::daemon(&a),
        Command::Start(a) => cmd::start(&a),
        Command::Stop(a) => cmd::stop(&a),
        Command::Restart(a) => cmd::restart(&a),
        Command::Enqueue(a) => cmd::enqueue(&a),
        Command::New(a) => cmd::new(&a),
        Command::Mktask(a) => cmd::mktask(&a),
        Command::Hold(a) => cmd::hold(&a),
        Command::Release(a) => cmd::release(&a),
        Command::Cancel(a) => cmd::cancel(&a),
        Command::Prio(a) => cmd::prio(&a),
        Command::Kill(a) => cmd::kill(&a),
        Command::Clean(a) => cmd::clean(&a),
        Command::Workers(a) => cmd::workers(&a),
        Command::Shutdown(a) => cmd::shutdown(&a),
        Command::Probe(a) => cmd::probe(&a),
        Command::Completions(a) => cmd::completions(&a),
        Command::Feedback(a) => feedback::cmd_feedback(&a),
        Command::Version(a) => cmd::version(&a),
        Command::Upgrade(a) => upgrade::cmd_upgrade(&a),
    }
}
