//! `client env`: push the row's env_setup/shell snippets to files
//! under `<qroot>` on the box.

use crate::server::registry::{self, ClientEntry, ResolvedQueue};
use crate::server::transport::{queue_exec, Transport};
use anyhow::{Context, Result};

/// `shunt client env <name>`: push the row's `env_setup`/`shell` to
/// `<qroot>/env_setup` + `<qroot>/shell` on the box. Empty fields
/// remove the files so the runner falls back to bash/none. The
/// row's residency preference is pushed the same way:
/// `exit_on_idle = true` plants the flag file that tells the daemon
/// to exit once the queue drains (§3.11a); false removes it.
pub fn push_env(t: &dyn Transport, name: &str) -> Result<()> {
    let entry = registry::load_client(name)?;
    let mut failures = Vec::new();
    for queue in registry::all_queues(&entry)? {
        if let Err(err) = push_resolved_env(t, &queue) {
            failures.push(format!("{}: {err:#}", queue.name));
        }
    }
    anyhow::ensure!(failures.is_empty(), "env push incomplete for {name}: {}", failures.join("; "));
    Ok(())
}

/// Push one queue's configured environment and optional worker preference.
pub fn push_queue_env(t: &dyn Transport, name: &str, queue: &str) -> Result<()> {
    let queue = registry::load_queue(name, Some(queue))?;
    push_resolved_env(t, &queue)
}

pub(super) fn push_resolved_env(t: &dyn Transport, queue: &ResolvedQueue) -> Result<()> {
    let e = &queue.client;
    write_qroot_file(t, e, "env_setup", &e.env_setup)?;
    write_qroot_file(t, e, "shell", &e.shell)?;
    write_qroot_file(t, e, "exit_on_idle", if e.exit_on_idle { "1\n" } else { "" })?;
    // An unset preference must not reset a live, manually tuned worker pool.
    if let Some(workers) = queue.workers {
        queue_exec(t, queue, &["workers", "set", &workers.to_string(), "--json"], None).context("push queue workers")?;
    }
    crate::server::tunnel::sync_queue_env(t, &e.name, &queue.name).context("reconcile queue tunnel environment")?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::registry::{tests::HOME_LOCK, QueueEntry};
    use std::sync::Mutex;

    /// (queue root, argv, stdin) observed by the fake transport.
    type RecordedCall = (String, Vec<String>, Option<String>);

    #[derive(Default)]
    struct Recording(Mutex<Vec<RecordedCall>>);

    impl Transport for Recording {
        fn exec(&self, client: &ClientEntry, args: &[String], input: Option<&str>) -> Result<String> {
            self.0.lock().unwrap().push((client.qroot.clone(), args.to_vec(), input.map(str::to_string)));
            Ok(String::new())
        }

        fn copy_to(&self, _: &ClientEntry, _: &std::path::Path, _: &str) -> Result<()> {
            panic!("environment push must not deploy a binary")
        }
    }

    #[test]
    fn env_all_and_selected_respect_queue_workers() {
        let _g = HOME_LOCK.lock().unwrap();
        let home = std::env::temp_dir().join(format!("shunt-queue-env-{}", std::process::id()));
        std::env::set_var("SHUNT_HOME", &home);
        let mut entry = super::super::test_entry();
        entry.qroot.clear();
        entry.queues.insert("alpha".into(), QueueEntry { qroot: "/data/alpha".into(), env_setup: "export A=1".into(), shell: "bash".into(), exit_on_idle: true, workers: Some(5) });
        entry.queues.insert("beta".into(), QueueEntry { qroot: "/data/beta".into(), env_setup: String::new(), shell: String::new(), exit_on_idle: false, workers: None });
        registry::save_client(&entry).unwrap();
        let t = Recording::default();
        push_env(&t, "b").unwrap();
        {
            let calls = t.0.lock().unwrap();
            assert!(calls.iter().any(|(root, args, _)| root == "/data/alpha" && args.windows(3).any(|tail| tail == ["workers", "set", "5"])));
            assert!(!calls.iter().any(|(root, args, _)| root == "/data/beta" && args.iter().any(|arg| arg == "workers")));
            assert!(calls.iter().any(|(root, args, _)| root == "/data/beta" && args == &["rm", "-f", "/data/beta/env_setup"]));
        }
        t.0.lock().unwrap().clear();
        push_queue_env(&t, "b", "beta").unwrap();
        assert!(t.0.lock().unwrap().iter().all(|(root, _, _)| root == "/data/beta"));
        assert!(push_queue_env(&t, "b", "missing").is_err());
        let stored = registry::load_client("b").unwrap();
        assert!(stored.qroot.is_empty());
        assert_eq!(stored.queues.len(), 2);
        std::env::remove_var("SHUNT_HOME");
        std::fs::remove_dir_all(home).ok();
    }
}
