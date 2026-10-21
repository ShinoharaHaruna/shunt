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
//! - `compat`    — §15 glibc/musl check before the binary ships
//! - `release`   — §15 release-asset rescue (musl download + verify)
//!
//! Everything a caller outside `client_ops` needs is re-exported
//! here so `crate::server::client_ops::*` paths stay stable.

mod bootstrap;
mod compat;
mod env;
mod hygiene;
mod keys;
mod lifecycle;
mod probe;
mod release;
mod update;

pub use bootstrap::bootstrap;
pub use env::push_env;
pub use hygiene::rekey;
pub use keys::{deploy_key, ensure_key};
pub use lifecycle::{remove, rename};
pub use probe::{probe, probe_facts};
pub use update::{update, EndpointUpdate};

/// Shared `ClientEntry` fixture for the submodules' unit tests —
/// every field spelled out so a new field forces one edit, not N.
#[cfg(test)]
pub(crate) fn test_entry() -> crate::server::registry::ClientEntry {
    crate::server::registry::ClientEntry { name: "b".into(), local: false, disabled: false, local_bin: None, bin_path: None, timeout_s: None, ssh_timeout_s: None, shutdown_cmd: None, expire: String::new(), env_setup: String::new(), shell: String::new(), endpoint_rotates: false, ssh: Default::default(), qroot: "/q".into(), deploy_root: "/data".into(), facts: Default::default(), billing: None }
}
