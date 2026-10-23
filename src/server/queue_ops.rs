//! Queue lifecycle cannot inherit client teardown: sibling queues share
//! the SSH identity, tunnel, host-key store, and connection state.

use crate::proto::wire::StatusReport;
use crate::server::registry::{self, ClientEntry, ResolvedQueue};
use crate::server::remote;
use crate::server::transport::{queue_exec, Transport};
use anyhow::{Context, Result};

fn status(t: &dyn Transport, queue: &ResolvedQueue) -> Result<StatusReport> {
    match remote::remote_status(t, queue) {
        remote::StatusAnswer::Report { report, .. } => Ok(*report),
        // A dead endpoint propagates bare (its error already names
        // the cause); an answered-but-wrong reply gets the
        // state-unknown remediation — same split as before.
        remote::StatusAnswer::Transport(e) | remote::StatusAnswer::ConnDown(e) => Err(e),
        remote::StatusAnswer::Drift(e) => Err(e).with_context(|| format!("cannot determine {}/{} state; run `shunt status -c {} --queue {}`", queue.client.name, queue.name, queue.client.name, queue.name)),
    }
}

/// A stopped sibling still owns its pending work. This preflight is not
/// synchronization with enqueue or an inventory of other users' workloads.
pub fn check_shutdown(t: &dyn Transport, client: &ClientEntry, selected_queue: &str) -> Result<()> {
    anyhow::ensure!(!client.shared, "client '{}' is marked shared — idle poweroff refused; clear shared only after confirming sole use", client.name);
    registry::resolve_queue(client, Some(selected_queue))?;
    for sibling in registry::all_queues(client)? {
        if sibling.name == selected_queue {
            continue;
        }
        let report = status(t, &sibling).with_context(|| format!("shutdown refused: sibling queue '{}' has unknown state", sibling.name))?;
        anyhow::ensure!(report.pending.is_empty() && report.running.is_empty(), "shutdown refused: sibling queue '{}' has pending or running work, even if its daemon is stopped; drain it before arming {}", sibling.name, selected_queue);
    }
    Ok(())
}

/// Deregister only one queue, without touching the instance's shared key.
/// Unknown runtime state is a refusal, not permission to destroy its files.
pub fn remove(t: &dyn Transport, client_name: &str, queue_name: &str, purge: bool) -> Result<()> {
    let client = registry::load_client(client_name)?;
    let queue = registry::resolve_queue(&client, Some(queue_name))?;
    anyhow::ensure!(registry::queue_names(&client).len() > 1, "cannot remove the last queue of {client_name}; use `shunt client rm {client_name}` to deregister the instance");
    anyhow::ensure!(client.default_queue.as_deref() != Some(queue_name), "queue '{queue_name}' is the configured default; select another with `shunt queue default <name> -c {client_name}` before removing it");
    if purge {
        crate::server::client_ops::check_purge_qroot(&queue.client.qroot)?;
    }
    for project_name in registry::list_names("projects")? {
        let project = registry::load_project(&project_name)?;
        anyhow::ensure!(!project.bind.iter().any(|binding| binding.client == client_name && binding.queue == queue_name), "queue {client_name}/{queue_name} is bound to project {project_name}; run `shunt project unbind {project_name} {client_name} --queue {queue_name}` first");
    }
    for entry in crate::server::spool::list()? {
        anyhow::ensure!(entry.client != client_name || entry.queue != queue_name, "queue {client_name}/{queue_name} has staged task {}; dispatch it or remove it with `shunt spool rm {}` first", entry.id, entry.id);
    }
    for mut record in crate::server::tasks::list()? {
        if record.client == client_name && record.queue == queue_name && record.outcome.is_none() {
            let observed = crate::server::tasks::observe(t, &client, &mut record)?;
            anyhow::ensure!(matches!(observed, crate::server::tasks::Observed::Done(_) | crate::server::tasks::Observed::Failed(_)), "queue {client_name}/{queue_name} has an unfinished or unresolved task {}; run `shunt task show {}` and resolve it before removing the queue", record.id, record.id);
        }
    }
    let report = status(t, &queue)?;
    anyhow::ensure!(report.pending.is_empty() && report.running.is_empty(), "queue {client_name}/{queue_name} has pending or running work; drain it before removing the queue");
    for tail in [&["shutdown", "off", "--json"][..], &["stop", "--json"][..]] {
        let out = queue_exec(t, &queue, tail, None).context("disarm and stop queue before removal")?;
        let result: crate::proto::wire::WireResult = crate::proto::wire::parse(&out)?;
        anyhow::ensure!(result.ok, "queue {client_name}/{queue_name} refused {}: {}; it remains registered", tail[0], result.error.as_deref().unwrap_or("no reason returned"));
    }
    // STOP is cooperative, not an acknowledgement that the daemon exited.
    // Keep the registry and qroot intact until the daemon relinquishes them.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let report = status(t, &queue)?;
        anyhow::ensure!(report.pending.is_empty() && report.running.is_empty(), "queue {client_name}/{queue_name} received work during removal; it remains registered — stop concurrent dispatch and retry");
        if !report.daemon.alive {
            break;
        }
        anyhow::ensure!(std::time::Instant::now() < deadline, "queue {client_name}/{queue_name} daemon has not stopped; it remains registered — wait for it to exit and retry queue rm");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    if purge {
        t.exec(&queue.client, &["rm".into(), "-rf".into(), "--".into(), queue.client.qroot.clone()], None).context("purge stopped queue")?;
    }
    registry::remove_queue_config(client_name, queue_name)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Reports {
        pending: bool,
        unknown: bool,
        calls: Mutex<Vec<String>>,
    }

    impl Transport for Reports {
        fn exec(&self, client: &ClientEntry, _argv: &[String], _stdin: Option<&str>) -> Result<String> {
            self.calls.lock().unwrap().push(client.qroot.clone());
            if self.unknown {
                anyhow::bail!("unreachable fixture");
            }
            let report = StatusReport { protocol: crate::proto::wire::PROTOCOL_VERSION, pending: if self.pending { vec!["0100_stopped-work".into()] } else { Vec::new() }, ..Default::default() };
            Ok(serde_json::to_string(&report)?)
        }
        fn copy_to(&self, _client: &ClientEntry, _local: &std::path::Path, _remote: &str) -> Result<()> {
            panic!("preflight must never copy a binary");
        }
    }

    fn client() -> ClientEntry {
        let mut client = crate::server::client_ops::test_entry();
        client.qroot.clear();
        for name in ["train", "eval"] {
            client.queues.insert(name.into(), registry::QueueEntry { qroot: format!("/data/{name}"), ..Default::default() });
        }
        client
    }

    #[test]
    fn stopped_sibling_pending_work_blocks_shutdown() {
        let t = Reports { pending: true, unknown: false, calls: Mutex::new(Vec::new()) };
        let error = check_shutdown(&t, &client(), "train").unwrap_err();
        assert!(error.to_string().contains("even if its daemon is stopped"), "{error:#}");
        assert_eq!(*t.calls.lock().unwrap(), ["/data/eval"]);
    }

    #[test]
    fn unknown_sibling_blocks_shutdown_but_drained_sibling_allows_preflight() {
        let t = Reports { pending: false, unknown: true, calls: Mutex::new(Vec::new()) };
        assert!(format!("{:#}", check_shutdown(&t, &client(), "train").unwrap_err()).contains("unknown state"));
        let t = Reports { pending: false, unknown: false, calls: Mutex::new(Vec::new()) };
        check_shutdown(&t, &client(), "train").unwrap();
        assert_eq!(*t.calls.lock().unwrap(), ["/data/eval"]);
    }

    #[test]
    fn shared_client_refuses_before_any_probe() {
        let t = Reports { pending: false, unknown: false, calls: Mutex::new(Vec::new()) };
        let mut client = client();
        client.shared = true;
        assert!(check_shutdown(&t, &client, "train").is_err());
        assert!(t.calls.lock().unwrap().is_empty());
    }
}
