//! SSH exec transport (design.md §5). All argv crossing SSH is
//! shell-quoted here; call sites never hand-roll quoting.

use crate::server::registry::ClientEntry;
use anyhow::Result;

/// Quote one arg for the remote shell (`%q` equivalent).
pub fn shell_quote(arg: &str) -> String {
    if arg.chars().all(|c| {
        c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | '/' | ':' | '=' | '+' | ',')
    }) && !arg.is_empty()
    {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', "'\\''"))
}

/// Run `shunt <args>` on the client, optionally piping `stdin_body`.
/// Returns remote stdout. Stub.
pub fn exec(_client: &ClientEntry, _args: &[String], _stdin_body: Option<&[u8]>) -> Result<String> {
    anyhow::bail!("transport::exec: not yet implemented")
}

/// Liveness check used by auto-heal: is `shuntd` running on the box?
/// Stub.
pub fn daemon_alive(_client: &ClientEntry) -> Result<bool> {
    anyhow::bail!("transport::daemon_alive: not yet implemented")
}

#[cfg(test)]
mod tests {
    use super::shell_quote;

    #[test]
    fn quote_safe_passthrough() {
        assert_eq!(shell_quote("runs/x.json"), "runs/x.json");
    }

    #[test]
    fn quote_spaces_and_quotes() {
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
    }
}
