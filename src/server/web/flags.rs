//! The `serve` clap surface and its effective options (design.md
//! §18). One `ServeFlags` type serves `run`/`up`/`restart`; every
//! field is an `Option` so the converge paths can tell an explicit
//! flag from a built-in default when comparing against a live
//! instance's own argv (that comparison lives in `lifecycle`).

pub struct ServeOpts {
    /// Listen address (default 127.0.0.1).
    pub bind: String,
    pub port: u16,
    /// Required once `bind` leaves loopback; checked as `?token=`,
    /// `x-shunt-token`, or `Authorization: Bearer`.
    pub token: Option<String>,
    /// Wire-probe cadence in seconds (the expensive half — one ssh
    /// exec per queue per tick).
    pub interval_s: u64,
}

pub const DEFAULT_BIND: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 8747;
pub const DEFAULT_INTERVAL_S: u64 = 10;

/// `run`/`up`/`restart` flags. All-Option fields: `None` = not
/// passed on the command line, so `up`/`restart` can tell an
/// explicit flag from a built-in default when converging against
/// a live instance's own argv.
#[derive(clap::Args, Clone)]
pub struct ServeFlags {
    /// Listen address (default 127.0.0.1 — reach a remote dev
    /// machine with `ssh -L PORT:127.0.0.1:PORT <dev>`)
    #[arg(long)]
    pub bind: Option<String>,
    /// Listen port (default 8747)
    #[arg(long)]
    pub port: Option<u16>,
    /// Access token — REQUIRED when --bind is not loopback. Checked
    /// as ?token=, the x-shunt-token header, or Authorization: Bearer
    #[arg(long)]
    pub token: Option<String>,
    /// Wire-probe cadence in seconds for the clients section (one
    /// ssh exec per queue per tick); local state updates every 2s
    /// (default 10)
    #[arg(long)]
    pub interval_s: Option<u64>,
}

impl ServeFlags {
    /// Effective options for actually launching: explicit flags
    /// over the built-in defaults. The converge paths in
    /// `up`/`restart` compare the explicit subset against the
    /// running argv instead.
    pub fn resolve(&self) -> ServeOpts {
        ServeOpts { bind: self.bind.clone().unwrap_or_else(|| DEFAULT_BIND.to_string()), port: self.port.unwrap_or(DEFAULT_PORT), token: self.token.clone(), interval_s: self.interval_s.unwrap_or(DEFAULT_INTERVAL_S) }
    }
}
