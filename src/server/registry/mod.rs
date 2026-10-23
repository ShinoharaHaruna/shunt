//! Registry: human-editable TOML rows under `~/.config/shunt/`
//! (`$SHUNT_HOME` overrides for tests). Load/save here is real, as
//! are the server subsystems that consume it (spool, dispatch,
//! transports, tunnel).
//!
//! Module map:
//! - `types` — row structs (`ClientEntry`, `ProjectEntry`, ...)
//! - `paths` — `shunt_home` + per-name path constructors
//! - `state` — ssh conn-failure streak counters
//! - `io` — load/save/list/remove + `localhost` materialization
//! - `tests` — unit tests + `HOME_LOCK` (serializes `SHUNT_HOME` users)

mod io;
pub mod names;
mod paths;
mod queues;
mod state;
mod types;

pub(crate) use io::load_client_row;
pub use io::{list_clients, list_names, load_client, load_project, load_server_config, remove, retarget_default_client, save_client, save_project};
pub use paths::{client_key_path, known_hosts_path, shunt_home, valid_name};
pub use queues::{add_queue, all_queues, canonicalize_queues, default_queue_name, load_queue, queue_names, remove_queue_config, resolve_queue, set_default_queue, ResolvedQueue, DEFAULT_QUEUE};
pub use state::{conn_fail_bump, conn_fail_read, conn_fail_reset, CONN_FAIL_ESCALATE};
pub use types::{Billing, Binding, ClientEntry, ProjectDefaults, ProjectEntry, QueueEntry, ServerConfig, SshEndpoint};

#[cfg(test)]
mod queue_tests;
#[cfg(test)]
pub(crate) mod tests;
