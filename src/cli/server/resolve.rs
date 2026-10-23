//! Client resolution: `-c` vs local queue vs `default_client`
//! (design.md §4.1).

use crate::cli::args::Cli;
use anyhow::Result;

/// No queue context: no --root flag and no SHUNT_ROOT env — the
/// call has nothing local to act on.
pub(crate) fn rootless(root: &Option<String>) -> bool {
    root.is_none() && std::env::var_os("SHUNT_ROOT").is_none()
}

/// A typed client reference (`-c`, `version -c`, `upgrade -c`,
/// `project bind <client>`): resolve through the registry name
/// middleware (design.md §4.1). A unique case-fold hit substitutes
/// the canonical spelling with a stderr note — picking a box to
/// talk to deserves a spoken word — and a miss carries did-you-mean
/// suggestions. (Configured names like `default_client` are not
/// typed input and do not come through here.)
pub(crate) fn canonical_client(typed: &str) -> Result<String> {
    use crate::server::registry::names::{self, Resolution};
    match names::resolve_client(typed)? {
        Resolution::Unknown { input, suggestions } => {
            // list_clients skips disabled rows, so a disabled name
            // lands here — name it disabled rather than unknown.
            if crate::server::registry::load_client_row(&input).is_ok_and(|r| r.disabled) {
                anyhow::bail!("client {input} is disabled");
            }
            anyhow::bail!("unknown client {input:?}{}", names::did_you_mean(&suggestions));
        }
        r => names::into_resolved(r, "client"),
    }
}

/// Client for a server-only command (submit, tunnel): `-c` wins,
/// else `server.toml` `default_client`; neither is an error
/// (design.md §4.1).
pub(crate) fn required_client(flag: &Option<String>) -> Result<String> {
    if let Some(c) = flag {
        return canonical_client(c);
    }
    crate::server::registry::load_server_config()?.default_client.ok_or_else(|| anyhow::anyhow!("no -c/--client given and no default_client in server.toml"))
}

/// Client for a dual-mode command (`-c` or the local queue): `-c`
/// wins; a known local root keeps the call local; only when there
/// is nothing local to act on does `default_client` fill in — so a
/// server shell needn't repeat `-c`, and a box's own queue ops are
/// never hijacked by a stray server.toml (design.md §4.1).
pub(crate) fn resolve_client(flag: &Option<String>, root: &Option<String>) -> Result<Option<String>> {
    if let Some(c) = flag {
        return Ok(Some(canonical_client(c)?));
    }
    if !rootless(root) {
        return Ok(None);
    }
    Ok(crate::server::registry::load_server_config()?.default_client)
}

/// Bare invocations that cannot act print their own -h instead of
/// dying on 'queue root unknown' — a lost user learns the flags.
pub(crate) fn print_subhelp(sub: &str) -> Result<()> {
    use clap::CommandFactory;
    Cli::command().find_subcommand_mut(sub).expect(sub).print_help()?;
    Ok(())
}
