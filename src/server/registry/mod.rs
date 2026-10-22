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
mod paths;
mod state;
mod types;

pub(crate) use io::load_client_row;
pub use io::{list_clients, list_names, load_client, load_project, load_server_config, remove, retarget_default_client, save_client, save_project};
pub use paths::{client_key_path, known_hosts_path, shunt_home, valid_name};
pub use state::{conn_fail_bump, conn_fail_read, conn_fail_reset, CONN_FAIL_ESCALATE};
pub use types::{Billing, Binding, ClientEntry, ProjectDefaults, ProjectEntry, ServerConfig, SshEndpoint};

#[cfg(test)]
pub(crate) mod tests;
