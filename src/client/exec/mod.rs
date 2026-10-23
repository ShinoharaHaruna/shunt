//! Task spawn/finalize (design.md §3.7, §3.9, §3.12): process_group(0)
//! so pgid = leader pid, log header/trailer, done/failed split.

mod directives;
mod env;
mod finalize;
mod kill;
mod render;
mod resched;
mod spawn;
#[cfg(test)]
mod tests;

pub use directives::{task_directives, TaskDirectives};
pub use env::read_env_file;
pub use finalize::{finalize, quarantine, running_log, status_rc};
pub use kill::{kill_task, pgid_alive, pgid_is_task_group, pgid_is_task_leader, proc_pgrp_start, read_pid_file, signal_pgid, KillOutcome, SignalOutcome};
pub use render::render_task;
pub use resched::{maybe_reschedule, Resched, DEFAULT_RETRY_DELAY_S};
pub use spawn::spawn_task;
