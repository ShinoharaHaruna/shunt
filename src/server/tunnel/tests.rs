use super::env::env_write;
use super::heal::{heal_in, Heal};
use super::liveness::is_tunnel_cmdline;
use super::state::{client_lock, lock_in, pid_path, port_path, recorded_in, status_in, ON_LOCK_WAIT};
use super::updown::up_argv;
use super::{list, recorded, status};
use crate::server::registry::{self, ClientEntry, SshEndpoint};
use crate::server::transport::LocalTransport;
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn entry(root: &Path) -> ClientEntry {
    ClientEntry { name: "tun-test".into(), local: true, disabled: false, local_bin: None, bin_path: None, queues: Default::default(), default_queue: None, timeout_s: None, ssh_timeout_s: None, shutdown_cmd: None, expire: String::new(), env_setup: String::new(), shell: String::new(), endpoint_rotates: false, shared: false, exit_on_idle: false, ssh: SshEndpoint::default(), qroot: root.display().to_string(), deploy_root: "/".into(), facts: BTreeMap::new(), billing: None }
}

fn tmp(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("shunt-tun-{tag}-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
    crate::server::fsutil::private_dir(&dir).unwrap();
    dir
}

#[test]
fn proxy_reconciliation_covers_every_queue_and_preserves_other_env() {
    let dir = tmp("multi-env");
    let mut client = entry(&dir);
    client.qroot.clear();
    for name in ["train", "eval"] {
        let root = dir.join(name);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("env"), format!("MODE={name}\nALL_PROXY=stale\n")).unwrap();
        client.queues.insert(name.into(), registry::QueueEntry { qroot: root.display().to_string(), ..Default::default() });
    }
    let transport = LocalTransport::default();
    env_write(&transport, &client, "ALL_PROXY", Some("socks5h://127.0.0.1:2080")).unwrap();
    env_write(&transport, &client, "ALL_PROXY", Some("socks5h://127.0.0.1:2080")).unwrap();
    for name in ["train", "eval"] {
        let env = std::fs::read_to_string(dir.join(name).join("env")).unwrap();
        assert_eq!(env, format!("MODE={name}\nALL_PROXY=socks5h://127.0.0.1:2080\n"));
    }
    env_write(&transport, &client, "ALL_PROXY", None).unwrap();
    for name in ["train", "eval"] {
        assert_eq!(std::fs::read_to_string(dir.join(name).join("env")).unwrap(), format!("MODE={name}\n"));
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn queue_env_failure_does_not_skip_siblings() {
    let dir = tmp("multi-env-failure");
    let mut client = entry(&dir);
    client.qroot.clear();
    let blocked = dir.join("blocked");
    std::fs::write(&blocked, "not a directory").unwrap();
    client.queues.insert("bad".into(), registry::QueueEntry { qroot: blocked.display().to_string(), ..Default::default() });
    client.queues.insert("good".into(), registry::QueueEntry { qroot: dir.join("good").display().to_string(), ..Default::default() });
    let err = env_write(&LocalTransport::default(), &client, "ALL_PROXY", Some("socks5h://127.0.0.1:2080")).unwrap_err();
    assert!(err.to_string().contains("bad"), "{err:#}");
    assert_eq!(std::fs::read_to_string(dir.join("good/env")).unwrap(), "ALL_PROXY=socks5h://127.0.0.1:2080\n");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn locks_are_private_bounded_per_client_and_stable() {
    use std::os::unix::fs::MetadataExt;
    let _home = crate::server::registry::tests::HOME_LOCK.lock().unwrap();
    let _env = crate::server::transport::ENV_LOCK.lock().unwrap();
    let home = tmp("locks");
    let old_home = std::env::var_os("SHUNT_HOME");
    std::env::set_var("SHUNT_HOME", &home);
    let dir = home.join("tunnels");
    let first = lock_in(&dir, "one", Duration::ZERO).unwrap();
    let path = dir.join(".one.lock");
    let inode = std::fs::metadata(&path).unwrap().ino();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
    let err = lock_in(&dir, "one", Duration::ZERO).unwrap_err();
    assert!(err.to_string().contains("retry the tunnel command"));
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
    let other = lock_in(&dir, "two", Duration::ZERO).unwrap();
    drop(first);
    let next = lock_in(&dir, "one", Duration::ZERO).unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
    drop((other, next));
    match old_home {
        Some(home) => std::env::set_var("SHUNT_HOME", home),
        None => std::env::remove_var("SHUNT_HOME"),
    }
    std::fs::remove_dir_all(home).unwrap();
}

#[test]
fn state_readers_wait_for_complete_transaction() {
    let _home = crate::server::registry::tests::HOME_LOCK.lock().unwrap();
    let _env = crate::server::transport::ENV_LOCK.lock().unwrap();
    let home = tmp("readers");
    let old_home = std::env::var_os("SHUNT_HOME");
    std::env::set_var("SHUNT_HOME", &home);
    let dir = home.join("tunnels");
    let lock = client_lock(&dir, "remote").unwrap();
    // A writer has published only one half of a new record.
    crate::server::fsutil::write_private(&pid_path(&dir, "remote"), "99999999\n").unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let mut readers = Vec::new();
    for op in ["status", "recorded", "list"] {
        let tx = tx.clone();
        readers.push(std::thread::spawn(move || {
            ON_LOCK_WAIT.with(|observer| *observer.borrow_mut() = Some(Box::new(move || tx.send(()).unwrap())));
            match op {
                "status" => assert_eq!(status("remote").unwrap().port, Some(2080)),
                "recorded" => assert!(recorded("remote").unwrap()),
                "list" => assert_eq!(list().unwrap(), ["remote"]),
                _ => unreachable!(),
            }
        }));
    }
    for _ in &readers {
        rx.recv_timeout(Duration::from_secs(5)).expect("reader did not contend on the writer's independent flock");
    }
    crate::server::fsutil::write_private(&port_path(&dir, "remote"), "2080\n").unwrap();
    drop(lock);
    for reader in readers {
        reader.join().unwrap();
    }
    match old_home {
        Some(home) => std::env::set_var("SHUNT_HOME", home),
        None => std::env::remove_var("SHUNT_HOME"),
    }
    std::fs::remove_dir_all(home).unwrap();
}

#[test]
fn env_write_upserts_and_removes() {
    let root = tmp("env");
    let client = entry(&root);
    let t = LocalTransport { bin: PathBuf::from("/bin/sh"), timeout_s: Some(10) };
    let f = root.join("env");
    env_write(&t, &client, "ALL_PROXY", Some("socks5h://127.0.0.1:1080")).unwrap();
    env_write(&t, &client, "WORKDIR", Some("/work")).unwrap();
    let text = std::fs::read_to_string(&f).unwrap();
    assert!(text.contains("ALL_PROXY=socks5h://127.0.0.1:1080"));
    assert!(text.contains("WORKDIR=/work"));
    // Upsert replaces in place, does not duplicate.
    env_write(&t, &client, "ALL_PROXY", Some("socks5h://127.0.0.1:2080")).unwrap();
    let text = std::fs::read_to_string(&f).unwrap();
    assert_eq!(text.matches("ALL_PROXY=").count(), 1);
    assert!(text.contains(":2080"));
    // Remove drops only the named key.
    env_write(&t, &client, "ALL_PROXY", None).unwrap();
    let text = std::fs::read_to_string(&f).unwrap();
    assert!(!text.contains("ALL_PROXY"));
    assert!(text.contains("WORKDIR=/work"));
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn heal_not_recorded_is_noop() {
    let state = tmp("heal-nr");
    let root = tmp("heal-nr-q");
    let client = entry(&root);
    let t = LocalTransport { bin: PathBuf::from("/bin/sh"), timeout_s: Some(10) };
    let called = std::cell::Cell::new(false);
    let res = heal_in(&state, &t, &client, false, |_, _| {
        called.set(true);
        Ok(1)
    })
    .unwrap();
    assert!(matches!(res, Heal::NotRecorded));
    assert!(!called.get());
    std::fs::remove_dir_all(&state).ok();
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn heal_dead_tunnel_reconnects() {
    let state = tmp("heal-up");
    let root = tmp("heal-up-q");
    let client = entry(&root);
    let t = LocalTransport { bin: PathBuf::from("/bin/sh"), timeout_s: Some(10) };
    // Recorded + dead: port file only, no live pid.
    crate::server::fsutil::write_private(&port_path(&state, "tun-test"), "1080\n").unwrap();
    let res = heal_in(&state, &t, &client, false, |_, _| Ok(4242)).unwrap();
    assert!(matches!(res, Heal::Reconnected(4242)));
    std::fs::remove_dir_all(&state).ok();
    std::fs::remove_dir_all(&root).ok();
}

/// Live ssh but env missing ALL_PROXY (up's env_write failed):
/// heal must re-assert the line for the recorded port, not
/// report Healthy over a silent proxy bypass.
#[test]
fn heal_live_tunnel_repairs_env() {
    let state = tmp("heal-env");
    let root = tmp("heal-env-q");
    let client = entry(&root);
    let t = LocalTransport { bin: PathBuf::from("/bin/sh"), timeout_s: Some(10) };
    // Live tunnel recorded on 2080; env carries only a foreign
    // key — ALL_PROXY was never written (or was lost to an env
    // file rewrite on the box).
    crate::server::fsutil::write_private(&pid_path(&state, "tun-test"), "1234\n").unwrap();
    crate::server::fsutil::write_private(&port_path(&state, "tun-test"), "2080\n").unwrap();
    env_write(&t, &client, "WORKDIR", Some("/work")).unwrap();
    let res = heal_in(&state, &t, &client, true, |_, _| anyhow::bail!("must not reconnect a live tunnel")).unwrap();
    assert!(matches!(res, Heal::Healthy));
    let text = std::fs::read_to_string(root.join("env")).unwrap();
    assert!(text.contains("ALL_PROXY=socks5h://127.0.0.1:2080"), "{text}");
    assert!(text.contains("WORKDIR=/work"), "{text}");
    std::fs::remove_dir_all(&state).ok();
    std::fs::remove_dir_all(&root).ok();
}

/// Live tunnel but the port file is gone: the correct ALL_PROXY
/// line is unknowable, so heal strips rather than leave a line
/// that may point at a dead port.
#[test]
fn heal_live_tunnel_no_port_strips_env() {
    let state = tmp("heal-nop");
    let root = tmp("heal-nop-q");
    let client = entry(&root);
    let t = LocalTransport { bin: PathBuf::from("/bin/sh"), timeout_s: Some(10) };
    crate::server::fsutil::write_private(&pid_path(&state, "tun-test"), "1234\n").unwrap();
    env_write(&t, &client, "ALL_PROXY", Some("socks5h://127.0.0.1:1080")).unwrap();
    env_write(&t, &client, "WORKDIR", Some("/work")).unwrap();
    let res = heal_in(&state, &t, &client, true, |_, _| anyhow::bail!("must not reconnect a live tunnel")).unwrap();
    assert!(matches!(res, Heal::Healthy));
    let text = std::fs::read_to_string(root.join("env")).unwrap();
    assert!(!text.contains("ALL_PROXY"), "{text}");
    assert!(text.contains("WORKDIR=/work"), "{text}");
    std::fs::remove_dir_all(&state).ok();
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn heal_dead_tunnel_strips_proxy_on_failure() {
    let state = tmp("heal-strip");
    let root = tmp("heal-strip-q");
    let client = entry(&root);
    let t = LocalTransport { bin: PathBuf::from("/bin/sh"), timeout_s: Some(10) };
    // Poisoned env + dead tunnel record (pid file naming a dead pid).
    env_write(&t, &client, "ALL_PROXY", Some("socks5h://127.0.0.1:1080")).unwrap();
    env_write(&t, &client, "WORKDIR", Some("/work")).unwrap();
    crate::server::fsutil::write_private(&pid_path(&state, "tun-test"), "99999999\n").unwrap();
    crate::server::fsutil::write_private(&port_path(&state, "tun-test"), "1080\n").unwrap();
    let res = heal_in(&state, &t, &client, false, |_, _| anyhow::bail!("ssh refused")).unwrap();
    assert!(matches!(res, Heal::Stripped));
    let text = std::fs::read_to_string(root.join("env")).unwrap();
    assert!(!text.contains("ALL_PROXY"), "{text}");
    assert!(text.contains("WORKDIR=/work"), "{text}");
    // State files stay so the next sweep retries the reconnect,
    // and status reports the intermediate state as stale.
    assert!(recorded_in(&state, "tun-test"));
    assert!(status_in(&state, "tun-test", None).stale);
    std::fs::remove_dir_all(&state).ok();
    std::fs::remove_dir_all(&root).ok();
}

/// The tunnel ssh must ride NO mux: `ControlPath=none` (and
/// `ControlMaster=no`) must precede the inherited
/// `ControlPath=<cm>/<name>.sock` from `ssh_conn_args` — ssh
/// resolves each `-o` first-value-wins. Auth/known_hosts options
/// (-p, -i, UserKnownHostsFile) must survive.
#[test]
fn tunnel_argv_disables_mux_before_conn_args() {
    let _h = crate::server::registry::tests::HOME_LOCK.lock().unwrap();
    let _g = crate::server::transport::ENV_LOCK.lock().unwrap();
    let home = std::env::temp_dir().join(format!("shunt-tun-home-{}", std::process::id()));
    std::env::set_var("SHUNT_HOME", &home);
    let mut c = entry(Path::new("/q"));
    c.local = false;
    c.ssh.user = "u".into();
    c.ssh.host = "h".into();
    c.ssh.port = 2222;
    c.ssh.key = "/k".into();
    let argv = up_argv(&c, 1080).unwrap();
    std::env::remove_var("SHUNT_HOME");
    // First occurrence of each -o parameter is the effective one.
    let pos = |needle: &str| argv.iter().position(|a| a == needle).unwrap_or(usize::MAX);
    assert!(pos("ControlPath=none") < argv.iter().position(|a| a.starts_with("ControlPath=") && *a != "ControlPath=none").unwrap_or(usize::MAX), "{argv:?}");
    assert!(pos("ControlMaster=no") < pos("ControlMaster=auto"), "{argv:?}");
    assert!(argv.contains(&"/k".to_string()) && argv.contains(&"u@h".to_string()), "{argv:?}");
}

/// A bare `ssh -R` cmdline must NOT pass liveness when the
/// recorded port and user@host needles are absent — a stranger's
/// reverse forward recycling our pid would otherwise be TERM'd
/// by `tunnel down`.
#[test]
fn cmdline_needs_port_and_target_needles() {
    // Split argv elements as `proto::proc::split_argv` serves them.
    fn cl<'a>(args: &[&'a str]) -> Vec<&'a [u8]> {
        args.iter().map(|a| a.as_bytes()).collect()
    }
    let ours = cl(&["ssh", "-i", "key", "-N", "-R", "1080", "user@host"]);
    assert!(is_tunnel_cmdline(&ours, Some(1080), Some("user@host")));
    // Foreign -R on another port: rejected once the port is known.
    let foreign = cl(&["ssh", "-N", "-R", "9999", "other@box"]);
    assert!(!is_tunnel_cmdline(&foreign, Some(1080), Some("user@host")));
    assert!(!is_tunnel_cmdline(&foreign, Some(1080), None));
    // Same port, wrong target: still not ours.
    let wrong_target = cl(&["ssh", "-N", "-R", "1080", "other@box"]);
    assert!(!is_tunnel_cmdline(&wrong_target, Some(1080), Some("user@host")));
    // No recorded port: any `-R` element satisfies the port needle.
    assert!(is_tunnel_cmdline(&foreign, None, Some("other@box")));
    // argv0 basename match: /usr/bin/ssh counts, not-ssh does not.
    assert!(is_tunnel_cmdline(&cl(&["/usr/bin/ssh", "-R", "1080"]), Some(1080), None));
    assert!(!is_tunnel_cmdline(&cl(&["autossh", "-R", "1080"]), Some(1080), None));
}
