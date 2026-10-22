//! `client update`: endpoint drift (design.md §4.1). Rewrites the
//! row's ssh endpoint fields after the vendor moved the box and
//! cleans the connection state pinned to the dead endpoint.

use super::hygiene::{drop_hostkey, kill_mux};
use crate::server::registry::{self, ClientEntry};
use crate::server::transport;
use anyhow::Result;

/// Field-level changes for `client update` (endpoint drift, design.md
/// §4.1). Any unset Option leaves the row field untouched; an empty
/// `--password ""` clears it. `endpoint_rotates` is tri-state:
/// Some(true) marks, Some(false) clears a mislabeled row, None leaves
/// the flag alone — a one-way bool had no path back to false.
pub struct EndpointUpdate {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub user: Option<String>,
    pub password: Option<String>,
    pub endpoint_rotates: Option<bool>,
}

/// `shunt client update <name> [flags]`: rewrite the row's ssh
/// endpoint fields after the vendor moved the box. Editing the TOML
/// by hand leaves live state pinned to the dead endpoint, so when
/// host/port/user move this also drops the stale known_hosts entries
/// — BOTH the old and the new spec, because a rotating vendor
/// reassigns host:port tuples between tenants and the new endpoint
/// can carry a foreign key that hard-fails TOFU — kills the mux
/// master, tears down a recorded tunnel (best-effort ALL_PROXY
/// cleanup over the new endpoint), and resets the conn-fail streak.
/// Server-local: no reachability check; verify with `status -c`.
pub fn update(name: &str, u: EndpointUpdate) -> Result<()> {
    let endpoint_moved = u.host.is_some() || u.port.is_some() || u.user.is_some();
    if !endpoint_moved && u.password.is_none() && u.endpoint_rotates.is_none() {
        anyhow::bail!("nothing to update — pass --host/--port/--user/--password/--endpoint-rotates (other row fields — qroot, deploy_root, expire, env_setup — are hand-edited in clients/<name>.toml)");
    }
    if u.port == Some(0) {
        anyhow::bail!("--port 0 is not a usable endpoint");
    }
    let mut e = registry::load_client(name)?;
    let (old_host, old_port) = (e.ssh.host.clone(), e.ssh.port);
    let mut changes: Vec<String> = Vec::new();
    if let Some(h) = u.host {
        changes.push(format!("ssh.host: {old_host} -> {h}"));
        e.ssh.host = h;
    }
    if let Some(p) = u.port {
        changes.push(format!("ssh.port: {old_port} -> {p}"));
        e.ssh.port = p;
    }
    if let Some(user) = u.user {
        changes.push(format!("ssh.user: {} -> {user}", e.ssh.user));
        e.ssh.user = user;
    }
    if let Some(pw) = u.password {
        // Never echo the secret into terminal output or logs.
        changes.push(if pw.is_empty() { "ssh.password: cleared".into() } else { "ssh.password: set".into() });
        e.ssh.password = pw;
    }
    if let Some(v) = u.endpoint_rotates {
        if v != e.endpoint_rotates {
            changes.push(format!("endpoint_rotates: -> {v}"));
            e.endpoint_rotates = v;
        }
    }
    let path = registry::save_client(&e)?;
    println!("updated {name}: {}", changes.join(", "));
    println!("wrote {}", path.display());
    // Two rows at one host:port is legal (NAT fronts several boxes
    // behind one endpoint) but easy to do by accident — warn so a
    // mistaken duplicate is caught while an intentional one can
    // proceed. Per-client known_hosts keeps the keys from colliding;
    // distinct names in task submissions are what tell them apart.
    for peer in endpoint_peers(name, &e.ssh.host, e.ssh.port) {
        eprintln!("warn: client {peer} is also registered at {}:{} — if these are two boxes behind NAT, keep names distinct at submit time; if a duplicate, `shunt client rm` one", e.ssh.host, e.ssh.port);
    }
    if endpoint_moved && !e.local {
        if let Err(err) = drop_hostkey(&e.name, &old_host, old_port) {
            eprintln!("warn: could not drop old host key ({err:#})");
        }
        if let Err(err) = drop_hostkey(&e.name, &e.ssh.host, e.ssh.port) {
            eprintln!("warn: could not drop stale key for the new endpoint ({err:#})");
        }
        kill_mux(&e.name);
        if crate::server::tunnel::recorded(name)? {
            // Tears down the local forward and tries to strip
            // ALL_PROXY from <qroot>/env via the NEW endpoint; a
            // still-unreachable box keeps its stale proxy line —
            // warn rather than fail the update.
            let t = transport::transport_for(&e);
            if let Err(err) = crate::server::tunnel::down(t.as_ref(), &e) {
                eprintln!("warn: tunnel teardown incomplete ({err:#}); after reconnect, run `shunt tunnel -c {name} down` or remove ALL_PROXY from <qroot>/env");
            }
        }
        // Reset AFTER the tunnel's best-effort exec so a failed
        // reachability attempt cannot leave the streak at 1.
        registry::conn_fail_reset(&e.name);
        eprintln!("note: mux/known_hosts/fail-streak cleared for the old endpoint; verify with `shunt status -c {name}`");
        // A --user-only change keeps the deployed pubkey working
        // (authorized_keys is user-independent on the box side),
        // but a rebuilt container resets authorized_keys entirely —
        // the key is then gone even though the row still points at it.
        eprintln!("note: if the box was rebuilt (fresh authorized_keys), re-run `shunt client bootstrap {name}` to redeploy the key");
    }
    Ok(())
}

/// Names of OTHER client rows registered at the same host:port.
/// Duplicate endpoints are legal (NAT) but suspicious, so `update`
/// warns rather than rejects.
pub(crate) fn endpoint_peers(name: &str, host: &str, port: u16) -> Vec<String> {
    if host.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for n in registry::list_names("clients").unwrap_or_default() {
        if n == name {
            continue;
        }
        // Read the row directly — load_client would reject disabled
        // rows, but a disabled row at the same endpoint still confuses.
        if let Ok(text) = std::fs::read_to_string(registry::shunt_home().map(|h| h.join("clients").join(format!("{n}.toml"))).unwrap_or_default()) {
            if let Ok(c) = toml::from_str::<ClientEntry>(&text) {
                if !c.local && c.ssh.host == host && c.ssh.port == port {
                    out.push(n);
                }
            }
        }
    }
    out
}

/// Names of OTHER client rows pointing at the exact same remote
/// queue — same endpoint AND user AND qroot. Unlike endpoint peers
/// (legal behind NAT) this is never legitimate: two installs on one
/// qroot merge silently (§4.1 owner-claim also catches it at
/// bootstrap). `client add` warns so the collision is loud at
/// registration, not first detected at bootstrap.
pub fn same_queue_peers(name: &str, e: &ClientEntry) -> Vec<String> {
    if e.local || e.ssh.host.is_empty() {
        return Vec::new();
    }
    let roots = queue_roots(e);
    let mut out = Vec::new();
    for n in registry::list_names("clients").unwrap_or_default() {
        if n == name {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(registry::shunt_home().map(|h| h.join("clients").join(format!("{n}.toml"))).unwrap_or_default()) {
            if let Ok(c) = toml::from_str::<ClientEntry>(&text) {
                if !c.local && c.ssh.host == e.ssh.host && c.ssh.port == e.ssh.port && c.ssh.user == e.ssh.user && queue_roots(&c).iter().any(|root| roots.contains(root)) {
                    out.push(n);
                }
            }
        }
    }
    out
}

fn queue_roots(client: &ClientEntry) -> Vec<std::path::PathBuf> {
    let roots: Vec<&str> = if client.queues.is_empty() { vec![&client.qroot] } else { client.queues.values().map(|queue| queue.qroot.as_str()).collect() };
    roots.into_iter().filter(|root| !root.is_empty()).map(|root| std::path::Path::new(root).components().collect()).collect()
}

#[cfg(test)]
mod tests {
    use super::{endpoint_peers, ClientEntry};

    use super::super::test_entry as entry;

    #[test]
    fn endpoint_peers_finds_duplicate_endpoint() {
        let _g = crate::server::registry::tests::HOME_LOCK.lock().unwrap();
        let tmp = std::env::temp_dir().join(format!("shunt-peers-{}", std::process::id()));
        std::env::set_var("SHUNT_HOME", &tmp);
        let mut a = ClientEntry { name: "a".into(), ..entry() };
        a.ssh.host = "10.0.0.1".into();
        a.ssh.port = 2222;
        let mut b = ClientEntry { name: "b".into(), ..entry() };
        b.ssh.host = "10.0.0.1".into();
        b.ssh.port = 2222;
        let mut c = ClientEntry { name: "c".into(), ..entry() };
        c.ssh.host = "10.0.0.1".into();
        c.ssh.port = 3333; // same host, different port: not a peer
        let mut local = ClientEntry { name: "d".into(), ..entry() };
        local.local = true; // loopback rows never collide over ssh
        local.ssh.host = "10.0.0.1".into();
        local.ssh.port = 2222;
        for e in [&a, &b, &c, &local] {
            crate::server::registry::save_client(e).unwrap();
        }
        assert_eq!(endpoint_peers("a", "10.0.0.1", 2222), vec!["b"]);
        assert!(endpoint_peers("a", "", 0).is_empty());
        std::env::remove_var("SHUNT_HOME");
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn same_queue_peers_checks_every_queue_and_account() {
        let _g = crate::server::registry::tests::HOME_LOCK.lock().unwrap();
        let home = std::env::temp_dir().join(format!("shunt-queue-peers-{}", std::process::id()));
        std::env::set_var("SHUNT_HOME", &home);
        let mut a = entry();
        a.name = "a".into();
        a.ssh.host = "example.invalid".into();
        a.ssh.user = "u".into();
        crate::server::registry::canonicalize_queues(&mut a).unwrap();
        a.queues.insert("second".into(), crate::server::registry::QueueEntry { qroot: "/data/shared".into(), env_setup: String::new(), shell: String::new(), exit_on_idle: false, workers: None });
        let mut b = a.clone();
        b.name = "b".into();
        b.queues.clear();
        b.default_queue = None;
        b.qroot = "/data//./shared/".into();
        let mut c = b.clone();
        c.name = "c".into();
        c.ssh.user = "other".into();
        let mut d = b.clone();
        d.name = "d".into();
        d.ssh.port += 1;
        for e in [&a, &b, &c, &d] {
            crate::server::registry::save_client(e).unwrap();
        }
        assert_eq!(super::same_queue_peers("a", &a), ["b"]);
        assert_eq!(super::same_queue_peers("b", &b), ["a"]);
        std::env::remove_var("SHUNT_HOME");
        std::fs::remove_dir_all(home).ok();
    }
}
