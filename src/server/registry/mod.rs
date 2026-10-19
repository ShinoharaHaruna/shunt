//! Registry: human-editable TOML rows under `~/.config/shunt/`
//! (`$SHUNT_HOME` overrides for tests). Load/save here is real; the
//! rest of the server is stubbed.
//!
//! Module map:
//! - `types` — row structs (`ClientEntry`, `ProjectEntry`, ...)
//! - `paths` — `shunt_home` + per-name path constructors
//! - `privacy` — 0700/0600 enforcement (shared, `pub(crate)`)
//! - `state` — ssh conn-failure streak counters
//! - `io` — load/save/list/remove + `localhost` materialization
//! - `tests` — unit tests + `HOME_LOCK` (serializes `SHUNT_HOME` users)

mod io;
mod paths;
mod privacy;
mod state;
mod types;

pub use io::{list_clients, list_names, load_client, load_project, load_server_config, remove, save_client, save_project};
pub use paths::{client_key_path, known_hosts_path, shunt_home};
pub use state::{conn_fail_bump, conn_fail_read, conn_fail_reset, CONN_FAIL_ESCALATE};
pub use types::{Billing, Binding, ClientEntry, ProjectDefaults, ProjectEntry, ServerConfig, SshEndpoint};

pub(crate) use privacy::{private_dir, write_private};

#[cfg(test)]
pub(crate) mod tests;
