//! Server-side dispatch helpers: remote forwarding, client
//! resolution, registry mutations (client add, vendor templates),
//! and the task-ledger `task show` read path.

mod client_add;
mod remote;
mod resolve;
mod room;
mod task;
#[cfg(test)]
mod tests;

pub(crate) use client_add::{client_add, vendor_template_dirs, ClientAddOpts};
pub(crate) use remote::{remote, remote_queue};
pub(crate) use resolve::{canonical_client, print_subhelp, required_client, resolve_client, rootless};
pub(crate) use room::cmd_room;
pub(crate) use task::task_show;
