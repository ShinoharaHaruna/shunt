//! `client probe` and `client bootstrap` (design.md §4.1, §7, §8).
//! Both run over raw SSH with plain shell — the remote box may not
//! have a shunt binary yet, so the probe cannot use the wire protocol.

use crate::server::registry::{self, ClientEntry};
use crate::server::transport::{self, shunt_bin, Transport};
use anyhow::{Context, Result};
use serde_json::json;
use std::collections::BTreeMap;

/// Shell probe script, emitted as KEY=value lines. cgroup-aware with
/// v1 fallbacks; `nproc`/`free` only as last resort (design.md §8).
/// `$1` to `sh -c` is the deploy_root for the df measurement.
const PROBE_SCRIPT: &str = r#"
g=$(nvidia-smi --query-gpu=name,driver_version --format=csv,noheader 2>/dev/null | head -n1)
echo "GPU=$g"
c=$(nvidia-smi 2>/dev/null | sed -n 's/.*CUDA Version:[[:space:]]*\([0-9.]*\).*/\1/p' | head -n1)
echo "CUDA=$c"
cores=
if [ -f /sys/fs/cgroup/cpu.max ]; then
    read quota period < /sys/fs/cgroup/cpu.max
    [ "$quota" != "max" ] && [ -n "$period" ] && cores=$((quota / period))
elif [ -f /sys/fs/cgroup/cpu/cpu.cfs_quota_us ]; then
    q=$(cat /sys/fs/cgroup/cpu/cpu.cfs_quota_us); p=$(cat /sys/fs/cgroup/cpu/cpu.cfs_period_us)
    [ "$q" -gt 0 ] && [ "$p" -gt 0 ] && cores=$((q / p))
fi
[ -z "$cores" ] && cores=$(nproc 2>/dev/null)
echo "CORES=$cores"
mem=
if [ -f /sys/fs/cgroup/memory.max ]; then
    b=$(cat /sys/fs/cgroup/memory.max); [ "$b" != "max" ] && mem=$b
elif [ -f /sys/fs/cgroup/memory/memory.limit_in_bytes ]; then
    b=$(cat /sys/fs/cgroup/memory/memory.limit_in_bytes)
    [ "$b" -lt 9223372036854771712 ] && mem=$b
fi
[ -z "$mem" ] && mem=$(free -b 2>/dev/null | awk '/^Mem:/{print $2}')
echo "MEM_BYTES=$mem"
echo "DISK_FREE_GB=$(df --output=avail -BG "$1" 2>/dev/null | tail -n1 | tr -dc '0-9')"
cr=0
[ "$(id -u 2>/dev/null)" = "0" ] && cr=1
if [ "$cr" = "0" ] && sudo -n true 2>/dev/null; then cr=1; fi
echo "CAN_ROOT=$cr"
sc=
if [ -d /run/systemd/system ] && command -v systemctl >/dev/null 2>&1; then
    sc="systemctl poweroff"
elif command -v shutdown >/dev/null 2>&1; then
    sc="shutdown"
elif command -v poweroff >/dev/null 2>&1; then
    sc="poweroff"
fi
echo "SHUTDOWN_CMD=$sc"
echo "PY=$(python3 --version 2>&1 | awk '{print $2}')"
"#;

/// Run the raw-shell probe and parse KEY=value output into facts.
pub fn probe_facts(t: &dyn Transport, client: &ClientEntry) -> Result<BTreeMap<String, serde_json::Value>> {
    let root = if client.deploy_root.is_empty() { "/" } else { client.deploy_root.as_str() };
    let out = t.exec(client, &["sh", "-c", PROBE_SCRIPT, "sh", root].iter().map(|s| s.to_string()).collect::<Vec<_>>(), None)?;
    Ok(parse_probe(&out))
}

/// Split `GPU=name, driver` and coerce numeric fields.
fn parse_probe(out: &str) -> BTreeMap<String, serde_json::Value> {
    let mut m = BTreeMap::new();
    for line in out.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let v = v.trim();
        match k {
            "GPU" if !v.is_empty() => {
                let mut it = v.splitn(2, ',');
                m.insert("gpu".into(), json!(it.next().unwrap_or("").trim()));
                if let Some(d) = it.next() {
                    m.insert("driver".into(), json!(d.trim()));
                }
            }
            "CUDA" if !v.is_empty() => {
                m.insert("cuda".into(), json!(v));
            }
            "CORES" => {
                if let Ok(n) = v.parse::<u64>() {
                    m.insert("cores".into(), json!(n));
                }
            }
            "MEM_BYTES" => {
                if let Ok(n) = v.parse::<f64>() {
                    m.insert("mem_gb".into(), json!(((n / 1e9) * 10.0).round() / 10.0));
                }
            }
            "DISK_FREE_GB" => {
                if let Ok(n) = v.parse::<f64>() {
                    m.insert("disk_free_gb".into(), json!(n));
                }
            }
            "CAN_ROOT" => {
                m.insert("can_root".into(), json!(v == "1"));
            }
            "SHUTDOWN_CMD" if !v.is_empty() => {
                m.insert("shutdown_cmd".into(), json!(v));
            }
            "PY" if !v.is_empty() => {
                m.insert("python".into(), json!(v));
            }
            _ => {}
        }
    }
    m
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
        std::fs::create_dir_all(p)?;
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
    let kh = registry::known_hosts_path()?.display().to_string();
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
    // Conn-fail streak and tunnel state follow the identity.
    for (dir, ext) in [("state", "fail"), ("tunnels", "pid"), ("tunnels", "port"), ("tunnels", "log")] {
        let (from, to) = (home.join(dir).join(format!("{old}.{ext}")), home.join(dir).join(format!("{new}.{ext}")));
        if from.exists() {
            let _ = std::fs::rename(&from, &to);
        }
    }
    // Undelivered spool entries and project [[bind]] refs.
    for mut se in crate::server::spool::list()? {
        if se.client == old {
            se.client = new.into();
            let body = crate::server::spool::body(&se.id).unwrap_or_default();
            crate::server::spool::stage(&se, &body)?;
        }
    }
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

/// `ssh-keygen -R <spec>` scoped to shunt's own known_hosts. The
/// spec is `host` on port 22 else `[host]:port` — OpenSSH's stored
/// form. No-op when the store or the host is absent.
fn drop_hostkey(host: &str, port: u16) -> Result<()> {
    let kh = registry::known_hosts_path()?;
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
    drop_hostkey(&entry.ssh.host, entry.ssh.port)?;
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
        anyhow::bail!("nothing to update — pass --host/--port/--user/--password/--endpoint-rotates");
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
    if endpoint_moved && !e.local {
        if let Err(err) = drop_hostkey(&old_host, old_port) {
            eprintln!("warn: could not drop old host key ({err:#})");
        }
        if let Err(err) = drop_hostkey(&e.ssh.host, e.ssh.port) {
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
    }
    Ok(())
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
    let facts = probe_facts(t, &entry)?;
    entry.facts.extend(facts);

    let bin = shunt_bin(&entry);
    let bindir = bin.rsplit_once('/').map(|(d, _)| d).unwrap_or("").to_string();
    t.exec(&entry, &["mkdir".into(), "-p".into(), bindir.clone()], None).context("mkdir remote bin dir")?;
    let local = std::env::current_exe().context("current_exe")?;
    t.copy_to(&entry, &local, &bin).context("copy shunt binary")?;
    t.exec(&entry, &["chmod".into(), "+x".into(), bin.clone()], None)?;

    t.exec(&entry, &[bin.clone(), "init".into(), "--root".into(), entry.qroot.clone(), "--json".into()], None).context("remote shunt init")?;
    t.exec(&entry, &[bin.clone(), "start".into(), "--root".into(), entry.qroot.clone()], None).context("remote shunt start")?;

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
    use super::{parse_probe, REVOKE_SCRIPT};

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

    #[test]
    fn parse_sample_output() {
        let out = "GPU=NVIDIA GeForce RTX 4090, 535.54.03\nCUDA=12.2\nCORES=14\nMEM_BYTES=60129583104\nDISK_FREE_GB=118\nCAN_ROOT=1\nSHUTDOWN_CMD=systemctl poweroff\nPY=3.10.12\n";
        let f = parse_probe(out);
        assert_eq!(f["gpu"], "NVIDIA GeForce RTX 4090");
        assert_eq!(f["driver"], "535.54.03");
        assert_eq!(f["cuda"], "12.2");
        assert_eq!(f["cores"], 14);
        assert_eq!(f["mem_gb"], 60.1);
        assert_eq!(f["disk_free_gb"], 118.0);
        assert_eq!(f["python"], "3.10.12");
        assert_eq!(f["can_root"], true);
        assert_eq!(f["shutdown_cmd"], "systemctl poweroff");
    }

    #[test]
    fn parse_cpu_only_boot() {
        let out = "GPU=\nCUDA=\nCORES=8\nMEM_BYTES=\nDISK_FREE_GB=50\nPY=\n";
        let f = parse_probe(out);
        assert!(!f.contains_key("gpu"));
        assert_eq!(f["cores"], 8);
        assert!(!f.contains_key("mem_gb"));
    }
}
