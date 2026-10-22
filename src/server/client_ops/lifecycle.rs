//! Client deregistration: `rename` and `rm --purge` move/teardown
//! every piece of state a row name is load-bearing for.

use super::hygiene::rekey;
use crate::server::registry::{self, ClientEntry};
use crate::server::transport::{self, Transport};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

fn public_key_path(private_key: impl AsRef<Path>) -> PathBuf {
    // OpenSSH appends the suffix; replacing an extension aliases dotted names.
    let mut path = private_key.as_ref().as_os_str().to_os_string();
    path.push(".pub");
    PathBuf::from(path)
}

/// `shunt client rename <old> <new>`: registry names are
/// load-bearing — the row file, its `name` field, the dedicated
/// keypair, mux socket, conn-fail streak, tunnel state, queued
/// spool entries, and every project `[[bind]]` all move together.
/// Disabled rows rename too (we read the file directly, not
/// load_client, which would reject them).
pub fn rename(old: &str, new: &str) -> Result<()> {
    // Names are filenames — validate before any path is built.
    registry::valid_name(old)?;
    registry::valid_name(new)?;
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
        let (old_pub, new_pub) = (public_key_path(&old_key), public_key_path(&nk));
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
    super::hygiene::kill_mux(old);
    let old_sock = crate::server::transport::cm_dir()?.join(format!("{old}.sock"));
    let _ = std::fs::remove_file(&old_sock);
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
            // An unreadable body must not be staged back as an empty
            // task: skip the entry (it keeps pointing at the old name
            // and fails loudly at dispatch) rather than lose the task.
            match crate::server::spool::body(&se.id) {
                Ok(body) => {
                    se.client = new.into();
                    crate::server::spool::stage(&se, &body)?;
                }
                Err(e) => eprintln!("warn: spool entry {} body unreadable ({e:#}); left bound to {old} — fix or remove it manually", se.id),
            }
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
    // server.toml's default_client is part of the identity too
    // (§4.1): left pointing at `old` it fails every -c-less
    // command, and a later re-registration of `old` would silently
    // hijack the default. Best-effort — a broken server.toml must
    // not wedge the rename.
    if let Err(e) = registry::retarget_default_client(old, Some(new)) {
        eprintln!("warn: could not retarget default_client in server.toml ({e:#})");
    }
    println!("renamed {old} -> {new} ({})", written.display());
    Ok(())
}

/// `shunt client rm <name> [--purge]` — deregister a box.
///
/// Remote side (best-effort — `localhost` itself is refused above,
/// but other `local = true` rows are real execution targets):
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
    // Disabled rows remain removable, but neither the requested name nor
    // the row's inner identity may redirect teardown to another client.
    let e = registry::load_client_row(name)?;
    let home = registry::shunt_home()?;
    let row = home.join("clients").join(format!("{name}.toml"));

    // `--purge` becomes a remote `rm -rf <qroot>`; validate the path
    // before anything reaches a remote shell. A bad row must stop
    // the deregistration, not warn-and-continue — the remote steps
    // below are best-effort only after this gate passes.
    if purge {
        check_purge_qroot(&e.qroot)?;
    }

    // --- remote decommission (best-effort) ---
    // Local rows run the same steps over LocalTransport: the
    // loopback daemon is a real daemon and --purge a real rm -rf.
    if e.local || !e.ssh.host.is_empty() {
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
        // Local rows carry no ssh.key — nothing to revoke.
        let pub_path = public_key_path(&e.ssh.key);
        if !e.ssh.key.is_empty() {
            if let Ok(pubkey) = std::fs::read_to_string(&pub_path) {
                if let Some(blob) = pubkey.split_whitespace().nth(1) {
                    if let Err(err) = revoke_key_blob(t.as_ref(), &e, blob) {
                        eprintln!("warn: could not revoke authorized_keys ({err})");
                    }
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
        // Best-effort: `ssh-keygen -R` exits 1 when the spec is not
        // in the file (some OpenSSH builds), and a local hygiene
        // miss must not wedge the deregistration the way update.rs
        // already treats its drop_hostkey calls.
        if let Err(err) = rekey(&e) {
            eprintln!("warn: could not drop known_hosts entries ({err:#})");
        }
    }
    // After rekey's `ssh -O exit` ask: a dead master never removes
    // cm/<name>.sock itself — §4.1 counts it among the deregistered
    // state (rename already unlinks its old socket above).
    if let Ok(sock) = crate::server::transport::cm_dir().map(|d| d.join(format!("{name}.sock"))) {
        let _ = std::fs::remove_file(sock);
    }
    std::fs::remove_file(&row)?;
    let default_key = registry::client_key_path(name)?;
    if e.ssh.key == default_key.display().to_string() {
        let _ = std::fs::remove_file(&default_key);
        let _ = std::fs::remove_file(public_key_path(&default_key));
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
    // A removed client has no successor, so clear the key rather
    // than warn-and-leave: a dangling default_client would silently
    // bind a future re-registration of `name`, whereas removing it
    // makes required_client report the clean "no default" error.
    // Best-effort, same as rename.
    if let Err(e) = registry::retarget_default_client(name, None) {
        eprintln!("warn: could not clear default_client in server.toml ({e:#})");
    }
    println!("removed {name}");
    Ok(())
}

/// Guard for `rm --purge`'s remote `rm -rf <qroot>`: the row's qroot
/// is trusted input only after these checks. Rejects empty,
/// relative, `~`-prefixed, or `..`-carrying paths; anything shorter
/// than two components (`/`, `/root`, `/q`); a first component naming
/// a system dir (`/etc`, `/usr`, `/bin`, …); a home dir itself
/// (`/home`, `/home/<user>`); and suspiciously short paths. The
/// AutoDL default `/root/autodl-tmp/queue` passes — qroots under
/// /root are normal, only the top-level dirs are dangerous.
fn check_purge_qroot(qroot: &str) -> Result<()> {
    let p = qroot;
    anyhow::ensure!(!p.is_empty(), "--purge refused: qroot is empty");
    // The shell receives the original string, not a trimmed path.
    anyhow::ensure!(p == p.trim(), "--purge refused: qroot {p:?} has surrounding whitespace — edit the row to the exact absolute queue path");
    anyhow::ensure!(!p.starts_with('~'), "--purge refused: qroot {p:?} is ~-relative (expansion is shell-dependent)");
    anyhow::ensure!(p.starts_with('/'), "--purge refused: qroot {p:?} is not absolute");
    // `.` must not disguise a protected prefix or inflate the depth.
    let comps: Vec<&str> = p.split('/').filter(|c| !c.is_empty() && *c != ".").collect();
    anyhow::ensure!(!comps.contains(&".."), "--purge refused: qroot {p:?} contains ..");
    anyhow::ensure!(comps.len() >= 2, "--purge refused: qroot {p:?} is a top-level path — edit the row to a deeper queue dir");
    const SYSTEM_DIRS: [&str; 12] = ["bin", "sbin", "boot", "dev", "etc", "lib", "lib64", "proc", "sys", "usr", "run", "var"];
    anyhow::ensure!(!SYSTEM_DIRS.contains(&comps[0]), "--purge refused: qroot {p:?} lives under a system dir");
    anyhow::ensure!(!(comps[0] == "home" && comps.len() == 2), "--purge refused: qroot {p:?} is a user's home dir");
    anyhow::ensure!(comps.join("/").len() >= 7, "--purge refused: qroot {p:?} is suspiciously short for rm -rf");
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

#[cfg(test)]
mod tests {
    use super::{check_purge_qroot, REVOKE_SCRIPT};

    #[test]
    fn purge_qroot_accepts_real_queue_dirs() {
        for ok in ["/root/autodl-tmp/queue", "/data/queue", "/home/u/proj/queue", "/tmp/shunt-queue"] {
            assert!(check_purge_qroot(ok).is_ok(), "{ok} should be allowed");
        }
    }

    #[test]
    fn purge_qroot_rejects_dangerous_paths() {
        for bad in ["", "/", "/root", "/q", "home", "~/queue", "~root/q", "../queue", "/a/../q", "/home/u", "/home", "/etc/foo", "/usr/local/q", "/var/lib/x", "/opt/x"] {
            assert!(check_purge_qroot(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn purge_qroot_rejects_dot_disguised_protected_paths() {
        for bad in ["/./home/alice", "/home/./alice", "/home/alice/.", "/././", "/./root/./", "/./home/./", "/./etc/config", "/usr/./local/q", "/./opt/x"] {
            assert!(check_purge_qroot(bad).is_err(), "{bad:?} must be refused");
        }
        for dir in ["bin", "sbin", "boot", "dev", "etc", "lib", "lib64", "proc", "sys", "usr", "run", "var"] {
            let bad = format!("/./{dir}/queue");
            assert!(check_purge_qroot(&bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn purge_qroot_rejects_surrounding_whitespace() {
        for bad in [" /data/queue", "/data/queue ", "\t/data/queue", "/data/queue\n"] {
            assert!(check_purge_qroot(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn purge_qroot_accepts_deep_paths_with_dots_and_spaces() {
        for ok in ["/./root/autodl-tmp/queue", "/home/./alice/queue/.", "//data///./queue//", "/data/queue with spaces", "/data/.queue"] {
            assert!(check_purge_qroot(ok).is_ok(), "{ok:?} should be allowed");
        }
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
