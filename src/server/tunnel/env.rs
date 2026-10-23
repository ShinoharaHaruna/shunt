//! Box-side queue `env` reconciliation: upsert/remove KEY=value
//! lines over the client's transport.

use crate::server::registry::{self, ClientEntry};
use crate::server::transport::Transport;
use anyhow::Result;

use super::liveness::client_target;
use super::state::{client_lock, recorded_in, status_in, tunnels_dir};

/// Upsert or remove one KEY=value line in `<qroot>/env` on the box,
/// over the client's transport (works for ssh and loopback alike).
pub(super) fn env_write(t: &dyn Transport, client: &ClientEntry, key: &str, value: Option<&str>) -> Result<()> {
    let mut errors = Vec::new();
    for queue in registry::all_queues(client)? {
        if let Err(e) = env_write_one(t, &queue.client, key, value) {
            errors.push(format!("{}: {e:#}", queue.name));
        }
    }
    anyhow::ensure!(errors.is_empty(), "{} queue env reconciliation failed: {}; retry tunnel up/down", client.name, errors.join("; "));
    Ok(())
}

fn env_write_one(t: &dyn Transport, client: &ClientEntry, key: &str, value: Option<&str>) -> Result<()> {
    const SCRIPT: &str = r#"
f="$1/env"; k="$2"; v="$3"
mkdir -p "$1"; touch "$f"
t=$(mktemp "$1/.env.XXXXXX") || exit 1
trap 'rm -f "$t"' EXIT
grep -v "^$k=" "$f" > "$t" 2>/dev/null || true
[ -n "$v" ] && echo "$k=$v" >> "$t"
mv "$t" "$f"
"#;
    let argv = ["sh".to_string(), "-c".to_string(), SCRIPT.to_string(), "sh".to_string(), client.qroot.clone(), key.to_string(), value.unwrap_or("").to_string()];
    t.exec(client, &argv, None)?;
    Ok(())
}

/// A newly configured queue must not bypass a live client tunnel or inherit
/// a stale proxy. The same lock orders this edit against tunnel teardown.
pub fn sync_queue_env(t: &dyn Transport, client_name: &str, queue_name: &str) -> Result<()> {
    let client = registry::load_client(client_name)?;
    let queue = registry::resolve_queue(&client, Some(queue_name))?;
    let dir = tunnels_dir()?;
    let _lock = client_lock(&dir, client_name)?;
    if !recorded_in(&dir, client_name) {
        return Ok(());
    }
    let state = status_in(&dir, client_name, client_target(&client).as_deref());
    let proxy = if state.alive { state.port.map(|p| format!("socks5h://127.0.0.1:{p}")) } else { None };
    env_write_one(t, &queue.client, "ALL_PROXY", proxy.as_deref())
}
