//! End-to-end over the `localhost` loopback (design.md §4.1, §5):
//! server spool -> dispatcher -> real `shunt` binary -> filesystem
//! queue -> real daemon -> task executes -> done/. All inside one
//! tmp $SHUNT_HOME; no ssh, no remote box.

use anyhow::Result;
use shunt::server::transport::{queue_exec, LocalTransport};
use shunt::server::{dispatch, registry, spool};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Panic-safe cleanup: the test starts a real daemon; a mid-test
/// panic must not leave it running on a deleted tmp dir.
struct DaemonGuard(PathBuf);
impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let pidfile = self.0.join("daemon.pid");
        if let Ok(s) = std::fs::read_to_string(&pidfile) {
            if let Ok(pid) = s.trim().parse::<i32>() {
                // No libc dep: SIGKILL via `kill -9` is fine for cleanup.
                let _ = std::process::Command::new("kill").args(["-9", &pid.to_string()]).status();
            }
        }
        std::fs::remove_dir_all(&self.0).ok();
    }
}

#[test]
fn localhost_submit_dispatch_execute_done() -> Result<()> {
    // Own tmp registry; localhost qroot synthesizes to $SHUNT_HOME/queue.
    let home = std::env::temp_dir().join(format!("shunt-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&home)?;
    let _guard = DaemonGuard(home.join("queue"));
    std::env::set_var("SHUNT_HOME", &home);
    std::env::set_var("POLL", "0.2"); // faster daemon loop for the test

    let client = registry::load_client("localhost")?;
    assert!(client.local);
    assert_eq!(client.qroot, home.join("queue").display().to_string());
    let queue = registry::resolve_queue(&client, None)?;
    let qroot = PathBuf::from(&client.qroot);

    // Loopback transport execs the built binary, not the registry path.
    let t = LocalTransport {
        bin: PathBuf::from(env!("CARGO_BIN_EXE_shunt")),
        timeout_s: Some(60), // a stuck wire call fails instead of hanging
    };

    // init over the wire surface.
    queue_exec(&t, &queue, &["init", "--json"], None)?;
    assert!(qroot.join("pending").is_dir());

    // Server side: stage a task for localhost, then one dispatch sweep.
    // The sweep auto-heals (starts) the real daemon, then enqueues.
    spool::submit("localhost", None, None, Some("e2e"), None, &Default::default(), &["touch".into(), "e2e.done".into()])?;
    assert!(dispatch::sweep_with(&t)? >= 1);
    assert!(spool::list()?.is_empty(), "spool should drain on ack");

    // The daemon claims the task and runs it. The `touch` marker may
    // land up to one poll before the finalize mv into done/, so poll
    // on the done/ entry itself.
    let deadline = Instant::now() + Duration::from_secs(30);
    while qroot.join("done").read_dir()?.next().is_none() {
        anyhow::ensure!(Instant::now() < deadline, "task did not finish within 30s (see {})", qroot.join("daemon.log").display());
        std::thread::sleep(Duration::from_millis(200));
    }
    // workdir defaults to the queue root, so `touch e2e.done` landed
    // under <qroot> — proving the runner cwd, not just the claim.
    assert!(qroot.join("e2e.done").exists());

    // A bound project supplies per-task workdir + env; both travel
    // inside the task body as `# shunt:` directives (design.md §3.13).
    let workdir = home.join("work");
    std::fs::create_dir_all(&workdir)?;
    let mut env = std::collections::BTreeMap::new();
    env.insert("E2E_HELLO".to_string(), "world".to_string());
    registry::save_project(&registry::ProjectEntry { name: "proj".into(), desc: String::new(), repo: String::new(), defaults: Default::default(), bind: vec![registry::Binding { client: "localhost".into(), queue: "default".into(), workdir: workdir.display().to_string(), env }] })?;
    spool::submit("localhost", Some("proj"), None, Some("e2e2"), None, &Default::default(), &["sh".into(), "-c".into(), "touch proj-$E2E_HELLO".into()])?;
    assert!(dispatch::sweep_with(&t)? >= 1);
    let deadline = Instant::now() + Duration::from_secs(30);
    while !workdir.join("proj-world").exists() {
        anyhow::ensure!(Instant::now() < deadline, "bound task did not finish within 30s");
        std::thread::sleep(Duration::from_millis(200));
    }

    // Drain + exit the daemon; the Drop guard covers failure paths.
    let _ = queue_exec(&t, &queue, &["stop"], None);
    let deadline = Instant::now() + Duration::from_secs(10);
    while qroot.join("daemon.pid").exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}
