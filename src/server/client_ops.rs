//! `client probe` and `client bootstrap` (design.md §4.1, §7, §8).
//! Facts come over the wire (`shunt probe --json` on the box) —
//! bootstrap installs the binary before probing, so `agent::probe`
//! is the single probe implementation. On an unbootstrapped box the
//! call fails with the binary-drift guidance from `wire::extract`.

use crate::server::registry::{self, ClientEntry};
use crate::server::transport::{self, shunt_bin, Transport};
use anyhow::{Context, Result};
use std::collections::BTreeMap;

/// Run `shunt probe --json` on the box and turn the `ProbeReport`
/// into a facts map (protocol field and nulls dropped).
pub fn probe_facts(t: &dyn Transport, client: &ClientEntry) -> Result<BTreeMap<String, serde_json::Value>> {
    let root = if client.deploy_root.is_empty() { "/" } else { client.deploy_root.as_str() };
    let out = t.exec(client, &[shunt_bin(client), "probe".into(), "--deploy-root".into(), root.into(), "--json".into()], None)?;
    let report: crate::proto::wire::ProbeReport = crate::proto::wire::parse(&out).context("remote probe reply")?;
    let mut v = serde_json::to_value(&report)?;
    let obj = v.as_object_mut().expect("ProbeReport serializes to an object");
    obj.remove("protocol");
    obj.retain(|_, x| !x.is_null());
    Ok(obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
}

/// `shunt client probe <name>`: re-collect facts, merge into the row.
/// Roundtrip rewrites the TOML; comments are lost (documented in §4.1).
pub fn probe(t: &dyn Transport, name: &str) -> Result<()> {
    let mut entry = registry::load_client(name)?;
    let facts = probe_facts(t, &entry)?;
    entry.facts.extend(facts);
    let p = registry::save_client(&entry)?;
    println!("wrote {}", p.display());
    println!("{}", serde_json::to_string_pretty(&entry.facts)?);
    Ok(())
}

/// Generate (or reuse) the dedicated keypair at
/// `$SHUNT_HOME/keys/<name>` and point `ssh.key` at it. Called by
/// `client add` and `bootstrap` — ssh becomes registration-transparent.
pub fn ensure_key(entry: &mut ClientEntry) -> Result<()> {
    if entry.local {
        return Ok(()); // loopback needs no key
    }
    if !entry.ssh.key.is_empty() && std::path::Path::new(&entry.ssh.key).exists() {
        return Ok(()); // user-supplied or previously generated key
    }
    if !entry.ssh.key.is_empty() {
        // A set-but-missing path is a user error; surface it instead
        // of silently swapping in a fresh key.
        anyhow::bail!("ssh.key points at {} but the file is missing", entry.ssh.key);
    }
    let path = registry::client_key_path(&entry.name)?;
    if let Some(p) = path.parent() {
        registry::private_dir(p)?;
    }
    if !path.exists() {
        let st = std::process::Command::new("ssh-keygen").args(["-t", "ed25519", "-N", "", "-f", &path.display().to_string(), "-C", &format!("shunt-{}", entry.name)]).stdin(std::process::Stdio::null()).status().context("spawn ssh-keygen")?;
        if !st.success() {
            anyhow::bail!("ssh-keygen failed for {}", path.display());
        }
    }
    entry.ssh.key = path.display().to_string();
    Ok(())
}

/// Install `<key>.pub` on the box when `ssh.password` is set (AutoDL
/// default: password auth). One connection, password-only auth (no
/// pubkey offers — boxes with small MaxAuthTries drop the connection
/// after too many), pubkey appended via stdin with a `grep -qxF`
/// dedupe. Returns `true` when the key was deployed; `false` when no
/// password is configured and the `.pub` must be installed once via
/// the vendor console or a manual ssh-copy-id.
pub fn deploy_key(entry: &ClientEntry) -> Result<bool> {
    if entry.ssh.password.is_empty() {
        return Ok(false);
    }
    let have_sshpass = std::process::Command::new("sh").args(["-c", "command -v sshpass >/dev/null 2>&1"]).status().map(|s| s.success()).unwrap_or(false);
    if !have_sshpass {
        anyhow::bail!("ssh.password is set but `sshpass` is not installed — install it or deploy {}.pub manually", entry.ssh.key);
    }
    let pub_key = std::fs::read_to_string(format!("{}.pub", entry.ssh.key)).with_context(|| format!("cannot read {}.pub", entry.ssh.key))?.trim().to_string();
    let kh = registry::known_hosts_path(&entry.name)?.display().to_string();
    // ControlPath=none: the password-auth connection must not become
    // the mux master — later key-based execs would ride a session
    // authenticated the wrong way.
    let script = "umask 077; mkdir -p ~/.ssh; touch ~/.ssh/authorized_keys; k=$(cat); grep -qxF \"$k\" ~/.ssh/authorized_keys || echo \"$k\" >> ~/.ssh/authorized_keys";
    let mut child = std::process::Command::new("sshpass").args(["-p", &entry.ssh.password, "ssh", "-p", &entry.ssh.port.to_string(), "-o", "PreferredAuthentications=password", "-o", "PubkeyAuthentication=no", "-o", "NumberOfPasswordPrompts=1", "-o", "StrictHostKeyChecking=accept-new", "-o", &format!("UserKnownHostsFile={kh}"), "-o", "ControlPath=none", "-o", &format!("ConnectTimeout={}", entry.ssh_timeout_s.unwrap_or(15)), &format!("{}@{}", entry.ssh.user, entry.ssh.host), script]).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().context("spawn sshpass ssh")?;
    use std::io::Write;
    child.stdin.as_mut().context("stdin piped")?.write_all(pub_key.as_bytes())?;
    drop(child.stdin.take());
    let st = child.wait().context("wait sshpass")?;
    Ok(st.success())
}

/// `shunt client rename <old> <new>`: registry names are
/// load-bearing — the row file, its `name` field, the dedicated
/// keypair, mux socket, conn-fail streak, tunnel state, queued
/// spool entries, and every project `[[bind]]` all move together.
/// Disabled rows rename too (we read the file directly, not
/// load_client, which would reject them).
pub fn rename(old: &str, new: &str) -> Result<()> {
    anyhow::ensure!(old != "localhost" && new != "localhost", "localhost is the reserved loopback name");
    let home = registry::shunt_home()?;
    let old_row = home.join("clients").join(format!("{old}.toml"));
    let new_row = home.join("clients").join(format!("{new}.toml"));
    let text = std::fs::read_to_string(&old_row).with_context(|| format!("no client row {}", old_row.display()))?;
    let mut e: ClientEntry = toml::from_str(&text).with_context(|| format!("bad TOML in {}", old_row.display()))?;
    anyhow::ensure!(!new_row.exists(), "client {new} already exists");

    // Dedicated keypair moves only when ssh.key points at the managed
    // default path; a user-supplied key path stays untouched.
    let old_key = registry::client_key_path(old)?;
    if e.ssh.key == old_key.display().to_string() && old_key.exists() {
        let nk = registry::client_key_path(new)?;
        std::fs::rename(&old_key, &nk)?;
        let (old_pub, new_pub) = (old_key.with_extension("pub"), nk.with_extension("pub"));
        if old_pub.exists() {
            std::fs::rename(&old_pub, &new_pub)?;
        }
        e.ssh.key = nk.display().to_string();
    }

    e.name = new.into();
    let written = registry::save_client(&e)?; // writes clients/<new>.toml
    std::fs::remove_file(&old_row)?;

    // A live mux master still answers on the old socket path — kill
    // it; the next exec opens a fresh master under the new name.
    let old_sock = crate::server::transport::cm_dir()?.join(format!("{old}.sock"));
    if old_sock.exists() {
        let _ = std::process::Command::new("ssh").args(["-S", &old_sock.display().to_string(), "-O", "exit", "x"]).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
        let _ = std::fs::remove_file(&old_sock);
    }
    // Conn-fail streak and tunnel state follow the identity, as does
    // the per-client known_hosts file.
    for (dir, ext) in [("state", "fail"), ("tunnels", "pid"), ("tunnels", "port"), ("tunnels", "log")] {
        let (from, to) = (home.join(dir).join(format!("{old}.{ext}")), home.join(dir).join(format!("{new}.{ext}")));
        if from.exists() {
            let _ = std::fs::rename(&from, &to);
        }
    }
    let (kh_from, kh_to) = (home.join("known_hosts").join(old), home.join("known_hosts").join(new));
    if kh_from.exists() {
        let _ = std::fs::rename(&kh_from, &kh_to);
    }
    // Undelivered spool entries and project [[bind]] refs.
    for mut se in crate::server::spool::list()? {
        if se.client == old {
            se.client = new.into();
            let body = crate::server::spool::body(&se.id).unwrap_or_default();
            crate::server::spool::stage(&se, &body)?;
        }
    }
    // Delivered-task ledger records point at the live row too.
    crate::server::tasks::retarget_client(old, new);
    for pname in registry::list_names("projects")? {
        let mut pj = registry::load_project(&pname)?;
        let mut touched = false;
        for b in &mut pj.bind {
            if b.client == old {
                b.client = new.into();
                touched = true;
            }
        }
        if touched {
            registry::save_project(&pj)?;
        }
    }
    println!("renamed {old} -> {new} ({})", written.display());
    Ok(())
}

/// `shunt client rm <name> [--purge]` — deregister a box.
///
/// Remote side (best-effort, skipped for local/unreachable boxes):
/// stop the daemon, pull our pubkey from authorized_keys — matched
/// by the key blob, not the comment, so a renamed client still
/// revokes correctly — and with --purge, `rm -rf <qroot>` wipes the
/// queue state, env files, and the deployed binary. Bound workdirs
/// are printed for the human to decide; we never rm task data.
///
/// Local side (always): kill tunnel + mux master, then row file,
/// managed keypair, known_hosts entries, conn-fail streak, tunnel
/// state, queued spool entries, and every project [[bind]] ref.
/// An unreachable box warns but never wedges the deregistration.
pub fn remove(name: &str, purge: bool) -> Result<()> {
    anyhow::ensure!(name != "localhost", "localhost is the reserved loopback name");
    let home = registry::shunt_home()?;
    let row = home.join("clients").join(format!("{name}.toml"));
    let text = std::fs::read_to_string(&row).with_context(|| format!("no client row {}", row.display()))?;
    let e: ClientEntry = toml::from_str(&text).with_context(|| format!("bad TOML in {}", row.display()))?;

    // --- remote decommission (best-effort) ---
    if !e.local && !e.ssh.host.is_empty() {
        let t = transport::transport_for(&e);
        let _ = crate::server::tunnel::down(t.as_ref(), &e); // kill the -R forward if it is up
        if !e.qroot.is_empty() {
            let bin = format!("{}/bin/shunt", e.qroot);
            if let Err(err) = t.exec(&e, &[bin, "stop".into(), "--root".into(), e.qroot.clone()], None) {
                eprintln!("warn: remote stop failed ({err}); continuing deregistration");
            }
        }
        // Revoke our key by blob match — comments track the name at
        // keygen time, which a `client rename` leaves behind. The
        // blob rides stdin (fixed-string `grep -vF`): base64 blobs
        // legitimately contain `/`, which turns a `sed '/blob/d'`
        // address into garbage — sed then exits 0 having done
        // nothing, leaving our key in place.
        let pub_path = std::path::PathBuf::from(&e.ssh.key).with_extension("pub");
        if let Ok(pubkey) = std::fs::read_to_string(&pub_path) {
            if let Some(blob) = pubkey.split_whitespace().nth(1) {
                if let Err(err) = revoke_key_blob(t.as_ref(), &e, blob) {
                    eprintln!("warn: could not revoke authorized_keys ({err})");
                }
            }
        }
        if purge && !e.qroot.is_empty() {
            let script = format!("rm -rf {}", crate::server::transport::join_argv(std::slice::from_ref(&e.qroot)));
            if let Err(err) = t.exec(&e, &["sh".into(), "-c".into(), script], None) {
                eprintln!("warn: purge of {} failed ({err})", e.qroot);
            }
        }
        // Task data lives in bind workdirs — report, never delete.
        for pname in registry::list_names("projects")? {
            let pj = registry::load_project(&pname)?;
            for b in pj.bind.iter().filter(|b| b.client == name) {
                eprintln!("note: project {pname} bound workdir {} on {name} — task files left in place", b.workdir);
            }
        }
    }

    // --- local teardown ---
    if !e.local {
        rekey(&e)?; // drops OUR known_hosts entries + kills the mux master
    }
    std::fs::remove_file(&row)?;
    let default_key = registry::client_key_path(name)?;
    if e.ssh.key == default_key.display().to_string() {
        let _ = std::fs::remove_file(&default_key);
        let _ = std::fs::remove_file(default_key.with_extension("pub"));
    }
    for (dir, ext) in [("state", "fail"), ("tunnels", "pid"), ("tunnels", "port"), ("tunnels", "log")] {
        let _ = std::fs::remove_file(home.join(dir).join(format!("{name}.{ext}")));
    }
    let _ = std::fs::remove_file(registry::known_hosts_path(name)?);
    for se in crate::server::spool::list()? {
        if se.client == name {
            let _ = crate::server::spool::remove(&se.id);
        }
    }
    for pname in registry::list_names("projects")? {
        let mut pj = registry::load_project(&pname)?;
        let before = pj.bind.len();
        pj.bind.retain(|b| b.client != name);
        if pj.bind.len() != before {
            registry::save_project(&pj)?;
            eprintln!("project {pname}: unbound {name}");
        }
    }
    println!("removed {name}");
    Ok(())
}

/// The remote revoke snippet, factored out for testing. Reads the
/// key blob on stdin (no quoting surface at all), drops every
/// authorized_keys line containing it via fixed-string grep, and
/// rewrites through a temp file. `grep -vF` exits 1 when the output
/// is empty — that still means "revoked", so only rc≥2 (e.g.
/// missing file) propagates; rc 0/1 move the temp file in place.
fn revoke_key_blob(t: &dyn Transport, e: &ClientEntry, blob: &str) -> Result<()> {
    t.exec(e, &["sh".into(), "-c".into(), REVOKE_SCRIPT.into()], Some(blob))?;
    Ok(())
}

/// See `revoke_key_blob`. `b=$(cat)` takes the blob on stdin.
const REVOKE_SCRIPT: &str = r#"
f=~/.ssh/authorized_keys
b=$(cat)
[ -f "$f" ] || exit 0
grep -vF -- "$b" "$f" > "$f.tmp"
rc=$?
if [ "$rc" -le 1 ]; then
    mv "$f.tmp" "$f"
else
    rm -f "$f.tmp"
    exit "$rc"
fi
"#;

/// `ssh-keygen -R <spec>` scoped to one client's own known_hosts
/// file. The spec is `host` on port 22 else `[host]:port` —
/// OpenSSH's stored form. No-op when the store or the host is absent.
fn drop_hostkey(client: &str, host: &str, port: u16) -> Result<()> {
    let kh = registry::known_hosts_path(client)?;
    if host.is_empty() || !kh.exists() {
        return Ok(());
    }
    let spec = if port == 22 { host.to_string() } else { format!("[{host}]:{port}") };
    let st = std::process::Command::new("ssh-keygen").args(["-R", &spec, "-f", &kh.display().to_string()]).stdin(std::process::Stdio::null()).status().context("spawn ssh-keygen -R")?;
    if !st.success() {
        anyhow::bail!("ssh-keygen -R {spec} failed");
    }
    Ok(())
}

/// Kill the live mux master for `name`, if one is up (`ssh -O exit`).
/// A master answers for the destination it was opened to regardless
/// of the requested target — after an endpoint change or key
/// rotation it must go, or the next exec rides the stale session.
fn kill_mux(name: &str) {
    if let Ok(sock) = crate::server::transport::cm_dir().map(|d| d.join(format!("{name}.sock"))) {
        if sock.exists() {
            let _ = std::process::Command::new("ssh").args(["-S", &sock.display().to_string(), "-O", "exit", "x"]).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
        }
    }
}

/// `shunt client hostkey <name>`: drop the box's host-key entries
/// from OUR known_hosts (`ssh-keygen -R` scoped to it) and kill the
/// live mux master, so the next connect re-TOFUs a rebuilt container.
/// This is the escape hatch for `REMOTE HOST IDENTIFICATION HAS
/// CHANGED` — the user's own ~/.ssh/known_hosts is never involved.
pub fn rekey(entry: &ClientEntry) -> Result<()> {
    drop_hostkey(&entry.name, &entry.ssh.host, entry.ssh.port)?;
    // The mux master still holds a session verified against the OLD key.
    kill_mux(&entry.name);
    Ok(())
}

/// Field-level changes for `client update` (endpoint drift, design.md
/// §4.1). Any unset Option leaves the row field untouched; an empty
/// `--password ""` clears it.
pub struct EndpointUpdate {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub user: Option<String>,
    pub password: Option<String>,
    pub endpoint_rotates: bool,
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
    if !endpoint_moved && u.password.is_none() && !u.endpoint_rotates {
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
    if u.endpoint_rotates && !e.endpoint_rotates {
        changes.push("endpoint_rotates: -> true".into());
        e.endpoint_rotates = true;
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
fn endpoint_peers(name: &str, host: &str, port: u16) -> Vec<String> {
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

/// `shunt client env <name>`: push the row's `env_setup`/`shell` to
/// `<qroot>/env_setup` + `<qroot>/shell` on the box. Empty fields
/// remove the files so the runner falls back to bash/none.
pub fn push_env(t: &dyn Transport, name: &str) -> Result<()> {
    let e = registry::load_client(name)?;
    write_qroot_file(t, &e, "env_setup", &e.env_setup)?;
    write_qroot_file(t, &e, "shell", &e.shell)?;
    Ok(())
}

/// Write-or-remove one small file under `<qroot>` over the client's
/// transport. Empty content removes the file.
fn write_qroot_file(t: &dyn Transport, e: &ClientEntry, fname: &str, content: &str) -> Result<()> {
    let path = format!("{}/{}", e.qroot, fname);
    if content.is_empty() {
        t.exec(e, &["rm".into(), "-f".into(), path], None)?;
        return Ok(());
    }
    t.exec(e, &["mkdir".into(), "-p".into(), e.qroot.clone()], None)?;
    t.exec(e, &["sh".into(), "-c".into(), "cat > \"$0\"".into(), path], Some(content))?;
    Ok(())
}

/// `shunt client bootstrap <name>`: ensure key + try deploy, probe,
/// ship the local binary to `<qroot>/bin/shunt`, remote `init` +
/// `start`, push env_setup, merge facts.
pub fn bootstrap(t: &dyn Transport, name: &str) -> Result<()> {
    let mut entry = registry::load_client(name)?;
    if !entry.local && entry.qroot.is_empty() {
        anyhow::bail!("{}: qroot is empty — set it in the row (usually <deploy_root>/queue)", entry.name);
    }
    ensure_key(&mut entry)?;
    if deploy_key(&entry)? {
        eprintln!("key deployed: {}", entry.ssh.key);
    } else if !entry.local && entry.ssh.password.is_empty() {
        eprintln!("note: no ssh.password in the row — install {}.pub on the box once\n      (vendor console or: ssh-copy-id -i {}.pub -p {} {}@{})", entry.ssh.key, entry.ssh.key, entry.ssh.port, entry.ssh.user, entry.ssh.host);
    }
    let bin = shunt_bin(&entry);
    let bindir = bin.rsplit_once('/').map(|(d, _)| d).unwrap_or("").to_string();
    t.exec(&entry, &["mkdir".into(), "-p".into(), bindir.clone()], None).context("mkdir remote bin dir")?;
    let local = std::env::current_exe().context("current_exe")?;
    // Stage-then-mv: scp truncates the destination in place, which
    // fails ETXTBSY while the daemon runs the old binary. Bootstrap
    // doubles as the binary upgrade path (init/start are idempotent),
    // so it must be safe to re-run on a live box.
    let staged = format!("{bin}.new");
    t.copy_to(&entry, &local, &staged).context("copy shunt binary")?;
    t.exec(&entry, &["chmod".into(), "+x".into(), staged.clone()], None)?;
    t.exec(&entry, &["mv".into(), "-f".into(), staged, bin.clone()], None).context("install shunt binary")?;

    t.exec(&entry, &[bin.clone(), "init".into(), "--root".into(), entry.qroot.clone(), "--json".into()], None).context("remote shunt init")?;
    t.exec(&entry, &[bin.clone(), "start".into(), "--root".into(), entry.qroot.clone()], None).context("remote shunt start")?;

    // Probe AFTER the binary lands: the wire `shunt probe` is the
    // single facts implementation (design.md §8).
    let facts = probe_facts(t, &entry)?;
    entry.facts.extend(facts);

    // env_setup/shell only push when the row actually carries them;
    // an empty push would clobber a box-side edit, which is not the
    // contract bootstrap has with the registry.
    if !entry.env_setup.is_empty() || !entry.shell.is_empty() {
        if let Err(e) = push_env(t, name) {
            eprintln!("note: env_setup push failed: {e:#}");
        }
    }
    let p = registry::save_client(&entry)?;
    println!("bootstrapped {} ({})", entry.name, bin);
    println!("wrote {}", p.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{endpoint_peers, probe_facts, ClientEntry, REVOKE_SCRIPT};
    use crate::server::transport::Transport;

    /// Transport stub: answers any exec with a canned probe report
    /// and asserts the call really went to `shunt probe --json`.
    struct FakeProbe(String);
    impl Transport for FakeProbe {
        fn exec(&self, _c: &ClientEntry, argv: &[String], _stdin: Option<&str>) -> anyhow::Result<String> {
            assert!(argv.iter().any(|a| a == "probe"), "argv: {argv:?}");
            assert!(argv.iter().any(|a| a == "--json"), "argv: {argv:?}");
            Ok(self.0.clone())
        }
        fn copy_to(&self, _c: &ClientEntry, _l: &std::path::Path, _r: &str) -> anyhow::Result<()> {
            unimplemented!()
        }
    }

    fn entry() -> ClientEntry {
        ClientEntry { name: "b".into(), local: false, disabled: false, local_bin: None, timeout_s: None, ssh_timeout_s: None, shutdown_cmd: None, expire: String::new(), env_setup: String::new(), shell: String::new(), endpoint_rotates: false, ssh: Default::default(), qroot: "/q".into(), deploy_root: "/data".into(), facts: Default::default(), billing: None }
    }

    #[test]
    fn probe_facts_parses_wire_report() {
        let t = FakeProbe(r#"{"protocol":1,"gpu":"RTX 4090","driver":"535.54.03","cuda":"12.2","cores":14,"mem_gb":56.0,"disk_free_gb":118.0,"python":"3.10.12","can_root":true,"shutdown_cmd":"systemctl poweroff"}"#.into());
        let f = probe_facts(&t, &entry()).unwrap();
        assert_eq!(f["gpu"], "RTX 4090");
        assert_eq!(f["driver"], "535.54.03");
        assert_eq!(f["cuda"], "12.2");
        assert_eq!(f["cores"], 14);
        assert_eq!(f["mem_gb"], 56.0);
        assert_eq!(f["disk_free_gb"], 118.0);
        assert_eq!(f["python"], "3.10.12");
        assert_eq!(f["can_root"], true);
        assert_eq!(f["shutdown_cmd"], "systemctl poweroff");
        assert!(!f.contains_key("protocol"));
    }

    #[test]
    fn probe_facts_drops_nulls_and_rejects_drift() {
        // CPU-only boot: nulls are dropped, not stored as JSON null.
        let t = FakeProbe(r#"{"protocol":1,"cores":8,"gpu":null,"cuda":null,"can_root":false}"#.into());
        let f = probe_facts(&t, &entry()).unwrap();
        assert_eq!(f["cores"], 8);
        assert!(!f.contains_key("gpu"));
        assert_eq!(f["can_root"], false);
        // A stale remote binary is named drift, not a parse error.
        let bad = FakeProbe(r#"{"protocol":2,"cores":8}"#.into());
        assert!(probe_facts(&bad, &entry()).is_err());
    }

    /// Run REVOKE_SCRIPT against a fake HOME holding the given
    /// authorized_keys content; returns (rc, resulting file). The
    /// child's HOME env carries the sandbox — no set_var needed.
    fn run_revoke(home: &std::path::Path, key_file: Option<&str>, blob: &str) -> (i32, String) {
        let ssh_dir = home.join(".ssh");
        std::fs::create_dir_all(&ssh_dir).unwrap();
        if let Some(content) = key_file {
            std::fs::write(ssh_dir.join("authorized_keys"), content).unwrap();
        }
        use std::io::Write;
        let mut child = std::process::Command::new("sh").args(["-c", REVOKE_SCRIPT]).env("HOME", home).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
        child.stdin.as_mut().unwrap().write_all(blob.as_bytes()).unwrap();
        drop(child.stdin.take());
        let st = child.wait().unwrap();
        let after = std::fs::read_to_string(ssh_dir.join("authorized_keys")).unwrap_or_default();
        (st.code().unwrap_or(-1), after)
    }

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
    fn revoke_removes_blob_with_slash() {
        // ed25519 base64 blobs legitimately contain `/` — the old
        // `sed '/blob/d'` truncated its regex at the slash and
        // silently left the key (rc 0, garbage written instead).
        let home = std::env::temp_dir().join(format!("shunt-revoke-{}-a", std::process::id()));
        let blob = "AAAAC3NzaC1lZDI1NTE5AAAAIAb/ab/cd+ef1234";
        let (rc, after) = run_revoke(&home, Some(&format!("ssh-ed25519 {blob} shunt-test\nssh-rsa otherkey user@host\nssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOther shunt-other\n")), blob);
        assert_eq!(rc, 0);
        assert_eq!(after, "ssh-rsa otherkey user@host\nssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOther shunt-other\n");
        assert!(!home.join(".ssh/authorized_keys.tmp").exists());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn revoke_last_line_and_missing_file() {
        let home = std::env::temp_dir().join(format!("shunt-revoke-{}-b", std::process::id()));
        let blob = "AAAAC3NzaC1lZDI1NTE5AAAAIX";
        // Only our line present: grep -vF outputs nothing (rc 1) —
        // that still means revoked.
        let (rc, after) = run_revoke(&home, Some(&format!("ssh-ed25519 {blob} shunt-test\n")), blob);
        assert_eq!(rc, 0);
        assert_eq!(after, "");
        // Missing authorized_keys is a no-op, not an error.
        let (rc, _) = run_revoke(&home.join("nohome"), None, blob);
        assert_eq!(rc, 0);
        std::fs::remove_dir_all(&home).ok();
    }
}
