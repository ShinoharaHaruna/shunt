//! Remaining command handlers: server daemons (`dispatch`,
//! `serve`, `tunnel`) and machine-readable outputs (`probe`,
//! `completions`, `version`).

use crate::cli::args::{CompletionsArgs, DispatchArgs, ProbeArgs, ServeAction, ServeArgs, TunnelAction, TunnelArgs, VersionArgs};
use crate::cli::local::jprint;
use crate::cli::server::{canonical_client, print_subhelp, remote, required_client};
use crate::cli::Cli;
use anyhow::Result;
use clap::CommandFactory;

pub fn dispatch(a: &DispatchArgs) -> Result<()> {
    crate::server::dispatch::run(a.once)
}

pub fn serve(a: &ServeArgs) -> Result<()> {
    match &a.action {
        // Bare `shunt serve` teaches the subcommands — same
        // lost-user affordance as `dlog`/`init`.
        None => print_subhelp("serve"),
        Some(ServeAction::Run(f)) => crate::server::web::run(f.resolve()),
        Some(ServeAction::Up(f)) => crate::server::web::up(f),
        Some(ServeAction::Down) => crate::server::web::down(),
        Some(ServeAction::Restart(f)) => crate::server::web::restart(f),
        Some(ServeAction::Status { json }) => crate::server::web::status(*json),
    }
}

pub fn tunnel(a: &TunnelArgs) -> Result<()> {
    use crate::server::tunnel;
    let name = required_client(&a.client)?;
    match &a.action {
        TunnelAction::Up { port } => {
            let (c, t) = crate::server::transport::client_transport(&name)?;
            let pid = tunnel::up(t.as_ref(), &c, *port)?;
            println!("tunnel up: {} pid {pid}", c.name);
            Ok(())
        }
        TunnelAction::Down => {
            let (c, t) = crate::server::transport::client_transport(&name)?;
            tunnel::down(t.as_ref(), &c)?;
            println!("tunnel down: {}", c.name);
            Ok(())
        }
        TunnelAction::Status => {
            let st = tunnel::status(&name)?;
            if st.alive {
                println!("{name}: up (socks5h://127.0.0.1:{})", st.port.map(|p| p.to_string()).unwrap_or_else(|| "?".into()));
            } else if st.stale {
                println!("{name}: down (stale state — the box's env may still export ALL_PROXY; `tunnel up` or `down` cleans it)");
            } else {
                println!("{name}: down");
            }
            Ok(())
        }
    }
}

pub fn probe(a: &ProbeArgs) -> Result<()> {
    let report = crate::client::probe::collect(&a.deploy_root);
    if a.json {
        jprint(&report);
    } else {
        match serde_json::to_string_pretty(&report) {
            Ok(s) => println!("{s}"),
            Err(e) => return crate::cli::local::jfail(a.json, &format!("{e:#}")),
        }
    }
    Ok(())
}

pub fn completions(a: &CompletionsArgs) -> Result<()> {
    let mut cmd = Cli::command();
    clap_complete::generate(a.shell, &mut cmd, "shunt", &mut std::io::stdout());
    Ok(())
}

pub fn version(a: &VersionArgs) -> Result<()> {
    if let Some(name) = &a.client {
        // Forward the --version flag, not a `version`
        // subcommand — remotes older than this feature
        // don't have one.
        let name = canonical_client(name)?;
        let out = remote(&name, &["--version"], None, false);
        return out;
    }
    if a.json {
        let sha = env!("SHUNT_GIT_SHA");
        let date = env!("SHUNT_GIT_DATE");
        println!("{}", serde_json::json!({"shunt": env!("SHUNT_VERSION"), "version": env!("CARGO_PKG_VERSION"), "sha": (!sha.is_empty()).then_some(sha), "date": (!date.is_empty()).then_some(date), "protocol": crate::proto::wire::PROTOCOL_VERSION, "os": std::env::consts::OS, "arch": std::env::consts::ARCH}));
    } else {
        println!("shunt {}", env!("SHUNT_VERSION"));
    }
    Ok(())
}
