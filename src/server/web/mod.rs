//! `shunt serve` (design.md §18): the read-only ops dashboard on the
//! dev machine. One `TcpListener` serves the embedded SPA, a small
//! `/api/` surface, and the `/ws` fan-out; two broadcast loops merge
//! into one snapshot — the same truth `status --all` computes, for
//! a browser instead of a terminal.
//!
//! One concern per file: `flags` the clap surface, `lifecycle` the
//! pidfile verbs and their argv convergence, `guard` the bind/Host/
//! Origin/token gates, `routes` the dispatch table, `snapshot` the
//! section assembly, `http`/`ws`/`assets` the transports and the
//! baked-in frontend.

mod assets;
mod flags;
mod guard;
mod http;
mod lifecycle;
mod routes;
mod snapshot;
#[cfg(test)]
mod tests;
mod ws;

pub use flags::{ServeFlags, ServeOpts};
pub(crate) use lifecycle::handoff_if_running;
pub use lifecycle::{down, restart, status, up};

use anyhow::{Context as _, Result};
use serde_json::{json, Value};
use std::net::TcpListener;
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// The merged latest snapshot: broadcast loops update their own
/// section; every new conn gets it whole (ws channel head) or via
/// `/api/snapshot`.
type Latest = Arc<RwLock<Value>>;

/// Server-local sections refresh at this fixed cadence: fs reads
/// cost nothing next to a wire probe, so it is a constant.
const LOCAL_TICK_S: u64 = 2;

/// `/api/room/<name>` returns at most this many messages (newest of
/// the merged live+archive stream).
const ROOM_MSG_TAIL: usize = 200;

pub fn run(opts: ServeOpts) -> Result<()> {
    guard::ensure_bind_allowed(&opts)?;

    let latest: Latest = Arc::new(RwLock::new(json!({
        "type": "snapshot",
        "protocol": crate::proto::wire::PROTOCOL_VERSION,
        "server": env!("SHUNT_VERSION"),
        "clients": Value::Null,
        "local": Value::Null,
        "ts": Value::Null,
    })));
    let hub = ws::Hub::new();

    // Slow sweep: wire probes at --interval. Clients stay null in
    // the snapshot until the first sweep lands — the UI shows local
    // state instead of an empty fleet.
    {
        let (latest, hub) = (latest.clone(), hub.clone());
        let interval = opts.interval_s.max(2);
        std::thread::spawn(move || loop {
            let clients = snapshot::clients_section();
            publish(&latest, &hub, |s| s["clients"] = clients.clone());
            std::thread::sleep(Duration::from_secs(interval));
        });
    }
    // Fast sweep: server-local fs state.
    {
        let (latest, hub) = (latest.clone(), hub.clone());
        std::thread::spawn(move || loop {
            publish(&latest, &hub, |s| s["local"] = snapshot::local_section());
            std::thread::sleep(Duration::from_secs(LOCAL_TICK_S));
        });
    }

    let listener = TcpListener::bind((opts.bind.as_str(), opts.port))?;
    let token_note = if opts.token.is_some() { " (append ?token=… to the URL)" } else { "" };
    println!("shunt serve: http://{}:{}{token_note} — read-only dashboard (fixed probes only); Ctrl-C stops", opts.bind, opts.port);

    // Lifecycle bookkeeping: pidfile first (up/down find us through
    // it), then a signal thread that removes it and exits — TERM
    // and INT both count as a clean stop.
    let pidf = lifecycle::pid_file()?;
    if let Some(dir) = pidf.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    std::fs::write(&pidf, format!("{}", std::process::id()))?;
    let verf = lifecycle::ver_file()?;
    std::fs::write(&verf, format!("{}\n", env!("SHUNT_VERSION")))?;
    // TERM/INT remove the lifecycle files and exit. USR2 re-execs
    // this image in place (§3.5a): the listener is CLOEXEC, so
    // execve drops it and the new image rebinds the same port — a
    // sub-second refusal window plus one dropped WS conn, never a
    // drain. An exec failure keeps the old image running.
    {
        let verf = verf.clone();
        let argv_rest: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
        let mut sigs = signal_hook::iterator::Signals::new([signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT, signal_hook::consts::SIGUSR2]).context("cannot install signal handlers")?;
        std::thread::spawn(move || {
            for sig in sigs.forever() {
                if sig == signal_hook::consts::SIGUSR2 {
                    eprintln!("serve: SIGUSR2 — re-exec in place");
                    let Err(e) = crate::handoff::exec_self(&argv_rest);
                    eprintln!("serve: handoff failed: {e} — continuing on the old image");
                } else {
                    let _ = std::fs::remove_file(&pidf);
                    let _ = std::fs::remove_file(&verf);
                    std::process::exit(0);
                }
            }
        });
    }
    crate::handoff::unblock_usr2();
    if std::env::var_os(crate::handoff::HANDOFF_ENV).is_some() {
        std::env::remove_var(crate::handoff::HANDOFF_ENV);
        eprintln!("serve: re-exec'd via handoff");
    }

    let auth = Arc::new(opts.token);
    let bind = opts.bind;
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let (latest, hub, auth, bind) = (latest.clone(), hub.clone(), auth.clone(), bind.clone());
                std::thread::spawn(move || {
                    let _ = routes::handle(stream, &latest, &hub, &auth, &bind);
                });
            }
            Err(e) => eprintln!("serve: accept failed: {e}"),
        }
    }
    Ok(())
}

/// Merge one section in and push the whole snapshot — the payload
/// is a few KB, so a full resend beats diffing.
fn publish(latest: &Latest, hub: &ws::Hub, f: impl FnOnce(&mut Value)) {
    let text = {
        let mut g = latest.write().unwrap();
        f(&mut g);
        g["ts"] = json!(crate::proto::time::rfc3339_z_now());
        serde_json::to_string(&*g).unwrap_or_default()
    };
    hub.broadcast(&text);
}
