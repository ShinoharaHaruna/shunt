//! `client update` keeps registry edits and the connection state they
//! invalidate together (design.md §4.1). Notes never invalidate a connection.

use super::hygiene::{drop_askpass, drop_hostkey, kill_mux};
use crate::server::registry::{self, ClientEntry};
use crate::server::transport;
use anyhow::Result;
use std::collections::BTreeMap;

/// Field-level changes for `client update` (design.md §4.1). Any unset
/// Option leaves the row field untouched; empty `--key ""` and
/// `--password ""` clear their fields. `endpoint_rotates` is tri-state:
/// Some(true) marks, Some(false) clears a mislabeled row, None leaves
/// the flag alone — a one-way bool had no path back to false.
/// The notes fields are dev-local registry data: they never join the
/// endpoint/connection hygiene below (mux kill, hostkey drop, tunnel
/// teardown, conn-fail reset).
pub struct EndpointUpdate {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub user: Option<String>,
    pub key: Option<String>,
    pub password: Option<String>,
    pub pre_connect: Option<String>,
    pub endpoint_rotates: Option<bool>,
    /// `Some` replaces ssh.options wholesale (empty clears it);
    /// `None` leaves the list alone. Managed so a changed list kills
    /// the mux master — a hand-edit would ride the stale one.
    pub options: Option<Vec<String>>,
    /// `Some` merges note pairs into the row (set/overwrite per key).
    pub notes: Option<BTreeMap<String, String>>,
    /// `Some` removes these note keys; absent keys are a no-op.
    /// Applied after `notes` so a same-call set+remove ends removed.
    pub notes_rm: Option<Vec<String>>,
    /// The CLI excludes merges and removals when clearing the whole map.
    pub notes_clear: bool,
}

/// Hand-editing an endpoint leaves live connection state pinned to its
/// old identity. Host, port, and user updates clear both endpoint host-key
/// entries because vendors can reassign a tuple carrying a foreign key.
/// They also kill the mux, tear down recorded tunnels with best-effort
/// ALL_PROXY cleanup, and reset the fail streak. Key and option changes
/// kill the mux. Password updates drop the cached askpass script. Connection
/// settings reset the fail streak, while notes leave connection state alone.
/// This server-local operation makes no reachability check. Verify endpoint
/// changes with `shunt status -c <name>`.
pub fn update(name: &str, u: EndpointUpdate) -> Result<()> {
    let endpoint_moved = u.host.is_some() || u.port.is_some() || u.user.is_some();
    // Notes do not invalidate connection state. The separate row-mtime
    // retry policy in conn_fail_streak still applies to any saved row.
    let conn_mutated = u.key.is_some() || u.password.is_some() || u.pre_connect.is_some() || u.endpoint_rotates.is_some() || u.options.is_some();
    if !endpoint_moved && !conn_mutated && u.notes.is_none() && u.notes_rm.is_none() && !u.notes_clear {
        anyhow::bail!("nothing to update: pass --host, --port, --user, --key, --password, --pre-connect, --option, --no-options, --endpoint-rotates, --no-endpoint-rotates, --note, --note-rm, or --no-notes. Manage queue fields with `shunt queue set`. Hand-edit deploy_root and expire in clients/<name>.toml");
    }
    if u.port == Some(0) {
        anyhow::bail!("--port 0 is not a usable endpoint");
    }
    let mut e = registry::load_client(name)?;
    let (old_host, old_port) = (e.ssh.host.clone(), e.ssh.port);
    let mut changes: Vec<String> = Vec::new();
    let mut key_changed = false;
    let mut options_changed = false;
    let mut password_touched = false;
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
    if let Some(key) = u.key {
        if key != e.ssh.key {
            e.ssh.key = key;
            changes.push(if e.ssh.key.is_empty() { "ssh.key: cleared".into() } else { "ssh.key: set".into() });
            key_changed = true;
        }
    }
    if let Some(pw) = u.password {
        // Never echo the secret into terminal output or logs.
        changes.push(if pw.is_empty() { "ssh.password: cleared".into() } else { "ssh.password: set".into() });
        password_touched = true;
        e.ssh.password = pw;
    }
    if let Some(pc) = u.pre_connect {
        // Never echo the snippet into terminal output — it can
        // legitimately carry a credential path or portal URL.
        if pc != e.pre_connect {
            changes.push(if pc.is_empty() { "pre_connect: cleared".into() } else { "pre_connect: set".into() });
            e.pre_connect = pc;
        }
    }
    if let Some(v) = u.endpoint_rotates {
        if v != e.endpoint_rotates {
            changes.push(format!("endpoint_rotates: -> {v}"));
            e.endpoint_rotates = v;
        }
    }
    if let Some(options) = u.options {
        // Never echo option values — a ProxyCommand can carry a
        // credential path or internal host (same rule as
        // pre_connect).
        if options != e.ssh.options {
            changes.push(if options.is_empty() { "ssh.options: cleared".into() } else { format!("ssh.options: set ({} entries)", options.len()) });
            e.ssh.options = options;
            options_changed = true;
        }
    }
    // Removals take precedence over sets of the same key in one call.
    // Keep values out of the update summary, as with connection settings.
    if u.notes_clear && !e.notes.is_empty() {
        changes.push(format!("notes: cleared ({} keys)", e.notes.len()));
        e.notes.clear();
    }
    if let Some(notes) = u.notes {
        for (k, v) in notes {
            if e.notes.get(&k) != Some(&v) {
                e.notes.insert(k.clone(), v);
                changes.push(format!("note {k}: set"));
            }
        }
    }
    if let Some(keys) = u.notes_rm {
        for k in keys {
            if e.notes.remove(&k).is_some() {
                changes.push(format!("note {k}: removed"));
            }
        }
    }
    let path = registry::save_client(&e)?;
    if (key_changed || options_changed) && !endpoint_moved {
        kill_mux(&e.name);
    }
    // kill_mux drops the generated askpass too, but a password-only
    // change takes no kill path: a rotated password would leave the
    // script pinned to the old secret, a cleared one would leave it
    // forever — it regenerates on the next password-only spawn.
    if password_touched {
        drop_askpass(&e.name);
    }
    if !endpoint_moved && conn_mutated {
        registry::conn_fail_reset(&e.name);
    }
    if !e.local && e.ssh.key.is_empty() && e.ssh.password.is_empty() {
        eprintln!("warn: client {name} has no auth path — set --key or --password");
    }
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
    // Options feed tunnels too (design.md §334): a recorded tunnel
    // still forwards under the old list. Surface it rather than
    // tearing a healthy forward down over an unrelated option — the
    // endpoint path above already tore it down when it mattered.
    if options_changed && !endpoint_moved && !e.local && crate::server::tunnel::recorded(name)? {
        eprintln!("warn: recorded tunnel still runs under the old ssh.options; apply them with `shunt tunnel -c {name} down` then `up`");
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
    use super::{endpoint_peers, ClientEntry, EndpointUpdate};

    use super::super::test_entry as entry;

    #[test]
    fn endpoint_peers_finds_duplicate_endpoint() {
        let _g = crate::server::registry::tests::HOME_LOCK.lock().unwrap();
        let tmp = crate::testutil::tmp_base().join(format!("shunt-peers-{}", std::process::id()));
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
    fn update_key_empty_clears_to_password_only_auth() {
        let _g = crate::server::registry::tests::HOME_LOCK.lock().unwrap();
        let home = crate::testutil::tmp_base().join(format!("shunt-update-key-{}", std::process::id()));
        std::env::set_var("SHUNT_HOME", &home);
        let mut client = entry();
        client.name = "key-clear".into();
        client.ssh.key = "/old/key".into();
        client.ssh.password = "pw".into();
        crate::server::registry::save_client(&client).unwrap();

        super::update("key-clear", EndpointUpdate { host: None, port: None, user: None, key: Some(String::new()), password: None, pre_connect: None, endpoint_rotates: None, options: None, notes: None, notes_rm: None, notes_clear: false }).unwrap();

        let saved = crate::server::registry::load_client("key-clear").unwrap();
        assert!(saved.ssh.key.is_empty());
        assert!(saved.ssh.password_only());
        std::env::remove_var("SHUNT_HOME");
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn update_options_replaces_then_clears() {
        let _g = crate::server::registry::tests::HOME_LOCK.lock().unwrap();
        let home = crate::testutil::tmp_base().join(format!("shunt-update-opts-{}", std::process::id()));
        std::env::set_var("SHUNT_HOME", &home);
        let mut client = entry();
        client.name = "opts".into();
        client.ssh.options = vec!["ProxyJump=old".into()];
        crate::server::registry::save_client(&client).unwrap();
        crate::server::registry::conn_fail_bump("opts");

        let bare = || EndpointUpdate { host: None, port: None, user: None, key: None, password: None, pre_connect: None, endpoint_rotates: None, options: None, notes: None, notes_rm: None, notes_clear: false };
        super::update("opts", EndpointUpdate { options: Some(vec!["ProxyJump=bastion".into(), "ForwardAgent=yes".into()]), ..bare() }).unwrap();
        assert_eq!(crate::server::registry::load_client("opts").unwrap().ssh.options, ["ProxyJump=bastion", "ForwardAgent=yes"]);
        assert_eq!(crate::server::registry::conn_fail_read("opts"), 0);
        crate::server::registry::conn_fail_bump("opts");
        super::update("opts", EndpointUpdate { options: Some(Vec::new()), ..bare() }).unwrap();
        assert!(crate::server::registry::load_client("opts").unwrap().ssh.options.is_empty());
        assert_eq!(crate::server::registry::conn_fail_read("opts"), 0);
        std::env::remove_var("SHUNT_HOME");
        std::fs::remove_dir_all(home).ok();
    }

    fn bare_update() -> EndpointUpdate {
        EndpointUpdate { host: None, port: None, user: None, key: None, password: None, pre_connect: None, endpoint_rotates: None, options: None, notes: None, notes_rm: None, notes_clear: false }
    }

    #[test]
    fn update_notes_merge_remove_and_clear() {
        let _g = crate::server::registry::tests::HOME_LOCK.lock().unwrap();
        let home = crate::testutil::tmp_base().join(format!("shunt-update-notes-{}", std::process::id()));
        std::env::set_var("SHUNT_HOME", &home);
        let mut client = entry();
        client.name = "notes".into();
        client.notes.insert("keep".into(), "old".into());
        client.notes.insert("stale".into(), "x".into());
        crate::server::registry::save_client(&client).unwrap();

        let mut u = bare_update();
        u.notes = Some(std::collections::BTreeMap::from([("keep".to_string(), "new".to_string()), ("gone".to_string(), "v".to_string()), ("fresh".to_string(), String::new())]));
        u.notes_rm = Some(vec!["gone".into(), "absent".into()]);
        super::update("notes", u).unwrap();
        let saved = crate::server::registry::load_client("notes").unwrap();
        assert_eq!(saved.notes.get("keep").map(String::as_str), Some("new"), "overwrite");
        assert_eq!(saved.notes.get("stale").map(String::as_str), Some("x"), "untouched key survives");
        assert_eq!(saved.notes.get("fresh").map(String::as_str), Some(""), "empty value is legal");
        assert!(!saved.notes.contains_key("gone"), "removals run after sets");

        let mut u = bare_update();
        u.notes_clear = true;
        super::update("notes", u).unwrap();
        assert!(crate::server::registry::load_client("notes").unwrap().notes.is_empty());
        std::env::remove_var("SHUNT_HOME");
        std::fs::remove_dir_all(home).ok();
    }

    /// Notes must leave endpoint state intact even when the client is down.
    #[test]
    fn notes_only_update_preserves_conn_fail_sidecar() {
        let _g = crate::server::registry::tests::HOME_LOCK.lock().unwrap();
        let home = crate::testutil::tmp_base().join(format!("shunt-notes-fail-{}", std::process::id()));
        std::env::set_var("SHUNT_HOME", &home);
        let mut client = entry();
        client.name = "failkeep".into();
        crate::server::registry::save_client(&client).unwrap();
        crate::server::registry::conn_fail_bump("failkeep");
        crate::server::registry::conn_fail_bump("failkeep");
        let sidecar = home.join("state").join("failkeep.fail");
        let before = std::fs::read_to_string(&sidecar).unwrap();
        let askpass = crate::server::transport::cm_dir().unwrap().join("failkeep.askpass");
        let known_hosts = crate::server::registry::known_hosts_path("failkeep").unwrap();
        let tunnel_port = home.join("tunnels").join("failkeep.port");
        for path in [&askpass, &known_hosts, &tunnel_port] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "preserve connection state\n").unwrap();
        }

        let updates = [EndpointUpdate { notes: Some(std::collections::BTreeMap::from([("datasets".to_string(), "/data/shared".to_string())])), ..bare_update() }, EndpointUpdate { notes_rm: Some(vec!["datasets".into(), "absent".into()]), ..bare_update() }, EndpointUpdate { notes_clear: true, ..bare_update() }];
        for u in updates {
            super::update("failkeep", u).unwrap();
            assert_eq!(crate::server::registry::conn_fail_read("failkeep"), 2);
            assert_eq!(std::fs::read_to_string(&sidecar).unwrap(), before, "sidecar bytes untouched");
            for path in [&askpass, &known_hosts, &tunnel_port] {
                assert_eq!(std::fs::read_to_string(path).unwrap(), "preserve connection state\n", "{} changed", path.display());
            }
        }

        // Notes in a mixed update must not suppress connection-state cleanup.
        let mut u = bare_update();
        u.notes = Some(std::collections::BTreeMap::from([("k".to_string(), "v".to_string())]));
        u.pre_connect = Some("true".into());
        super::update("failkeep", u).unwrap();
        assert_eq!(crate::server::registry::conn_fail_read("failkeep"), 0);
        assert!(!sidecar.exists());
        std::env::remove_var("SHUNT_HOME");
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn update_without_any_flag_still_bails() {
        let _g = crate::server::registry::tests::HOME_LOCK.lock().unwrap();
        let home = crate::testutil::tmp_base().join(format!("shunt-update-bail-{}", std::process::id()));
        std::env::set_var("SHUNT_HOME", &home);
        let mut client = entry();
        client.name = "bare".into();
        crate::server::registry::save_client(&client).unwrap();
        let err = super::update("bare", bare_update()).unwrap_err().to_string();
        assert!(err.contains("--note"), "{err}");
        std::env::remove_var("SHUNT_HOME");
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn same_queue_peers_checks_every_queue_and_account() {
        let _g = crate::server::registry::tests::HOME_LOCK.lock().unwrap();
        let home = crate::testutil::tmp_base().join(format!("shunt-queue-peers-{}", std::process::id()));
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
