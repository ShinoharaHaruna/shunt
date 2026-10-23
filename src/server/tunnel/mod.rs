//! Box-side SOCKS5 proxy via SSH reverse dynamic forward
//! (design.md §4.3): `ssh -N -R <port>` opens `127.0.0.1:<port>` as a
//! SOCKS5 listener ON THE BOX, tunnelled back through the dev
//! machine's network — the escape hatch for boxes whose outbound
//! connectivity is poor (e.g. mainland-China GPU vendors).
//!
//! `up` also writes `ALL_PROXY=socks5h://127.0.0.1:<port>` into the
//! queue's `env` file so tasks pick the proxy up transparently (the
//! runner strips ambient proxy vars, then injects the env file, so a
//! file-set proxy wins). `down` removes both.
//!
//! State under `$SHUNT_HOME/tunnels/`: `<name>.pid`, `<name>.port`,
//! `<name>.log`, plus a stable private `.<name>.lock`. Liveness is pidfile + `/proc` — same discipline as
//! the client daemon.

mod env;
mod heal;
mod liveness;
mod state;
#[cfg(test)]
mod tests;
mod updown;

pub use env::sync_queue_env;
pub use heal::{heal, Heal};
pub use state::{list, recorded, status, TunnelState};
pub use updown::{down, up};

pub const DEFAULT_SOCKS_PORT: u16 = 1080;
