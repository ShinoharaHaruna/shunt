//! `client` subcommand implementations (design.md §4.1, §7, §8).
//!
//! Module map:
//! - `keys`      — dedicated keypair: ensure/generate, password deploy
//! - `lifecycle` — `rename`, `rm --purge`, remote key revocation
//! - `update`    — `update` endpoint drift (+ EndpointUpdate)
//! - `hygiene`   — known_hosts drop + mux-master kill (`rekey`)
//! - `env`       — `env` push of env_setup/shell into <qroot>
//! - `probe`     — `probe` + wire `probe_facts`
//! - `bootstrap` — `bootstrap`: key, binary ship, init/start, facts
//!
//! Everything a caller outside `client_ops` needs is re-exported
//! here so `crate::server::client_ops::*` paths stay stable.

mod bootstrap;
mod env;
mod hygiene;
mod keys;
mod lifecycle;
mod probe;
mod update;

pub use bootstrap::bootstrap;
pub use env::push_env;
pub use hygiene::rekey;
pub use keys::{deploy_key, ensure_key};
pub use lifecycle::{remove, rename};
pub use probe::{probe, probe_facts};
pub use update::{update, EndpointUpdate};
