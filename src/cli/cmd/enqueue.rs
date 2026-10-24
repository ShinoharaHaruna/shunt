//! Local task intake: `enqueue` (install a task file), `new`
//! (wrap argv as a guarded task), `mktask` (render-only).

use crate::cli::args::{EnqueueArgs, NewArgs};
use crate::cli::local::{agent_enqueue, agent_new, jfail};
use crate::cli::server::{print_subhelp, rootless};
use anyhow::Result;

pub fn enqueue(a: &EnqueueArgs) -> Result<()> {
    if rootless(&a.root) {
        return print_subhelp("enqueue");
    }
    agent_enqueue(a)
}

pub fn new(a: &NewArgs) -> Result<()> {
    if rootless(&a.root) {
        return print_subhelp("new");
    }
    agent_new(a)
}

pub fn mktask(a: &NewArgs) -> Result<()> {
    // mktask can render for real: it is pure output. An empty
    // cmd would render `exec ` — a no-op task that exits 0
    // (task_meta refuses it before we get here).
    match a.task_meta() {
        Ok(meta) => {
            print!("{}", crate::client::exec::render_task(&meta));
            Ok(())
        }
        Err(e) => jfail(a.json, &format!("{e:#}")),
    }
}
