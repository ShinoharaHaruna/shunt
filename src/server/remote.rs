//! Wire-call middleware (design.md §5): the two classifications
//! every remote exec shares, owned once. Callers keep their own
//! loudness — the same discipline as `registry::names::Resolution`:
//! the enums describe what happened, each site decides how loud to
//! be about it.
//!
//! The conn_fail delta: `transport::ssh` bumps a per-client streak
//! when ssh exhausts its connect retries and resets it on success.
//! "This call burned a connection" is therefore a before/after
//! comparison, never an absolute read — a streak left standing by
//! an earlier call is not this call's evidence. `LocalTransport`
//! never touches the streak, so loopback execs only ever classify
//! Reply/RemoteErr; ConnDown is an ssh-only signal.

use crate::proto::wire::{self, StatusReport};
use crate::server::registry::{self, ResolvedQueue};
use crate::server::transport::{queue_exec, Transport};
use anyhow::Result;

/// Run a fallible call that may bump `client`'s conn_fail streak;
/// `burned` is true when the streak grew across the call — proof
/// the failure was a dead endpoint, shared by every sibling queue
/// on the same ssh identity (§4.4 backoff). The delta wraps the
/// whole closure, so a multi-exec op (tunnel heal) reports whether
/// ANY of its execs burned.
///
/// Not per-call attribution: a concurrent exec on the same client
/// (a parallel `status --all` probe, another sweep) that bumps
/// mid-call is indistinguishable. Accepted — "the endpoint is
/// down" is the same conclusion either way.
pub fn conn_fail_delta<T>(client: &str, call: impl FnOnce() -> Result<T>) -> (Result<T>, bool) {
    let before = registry::conn_fail_read(client);
    let r = call();
    (r, registry::conn_fail_read(client) > before)
}

/// One wire exec's outcome, classified by conn evidence.
pub enum WireCall {
    /// Remote answered; raw stdout for the caller's own wire parse.
    Reply(String),
    /// exec failed without conn evidence — a remote-side error
    /// (wire `{ok:false}` line, nonzero rc, exec deadline).
    RemoteErr(anyhow::Error),
    /// exec failed AND burned a conn_fail bump — a dead endpoint.
    ConnDown(anyhow::Error),
}

/// `queue_exec` + the conn_fail delta: one wire op on a resolved
/// queue, classified.
pub fn wire_call(t: &dyn Transport, queue: &ResolvedQueue, tail: &[&str], stdin: Option<&str>) -> WireCall {
    match conn_fail_delta(&queue.client.name, || queue_exec(t, queue, tail, stdin)) {
        (Ok(out), _) => WireCall::Reply(out),
        (Err(e), true) => WireCall::ConnDown(e),
        (Err(e), false) => WireCall::RemoteErr(e),
    }
}

/// A `status --json` answer, classified so each caller maps it to
/// its own verdict. The failure kinds are NOT interchangeable
/// (design.md §5's drift-vs-down rule): a dead endpoint, a live box
/// whose exec failed, and an answer that isn't wire-v1 are
/// different events — folding Drift into "down" would `start` a
/// drifted binary (dispatch) or report a live box unreachable
/// (status); folding ConnDown into Transport loses the
/// shared-outage signal the sweep's backoff needs.
pub enum StatusAnswer {
    /// Answered and parsed. `raw` is the extracted wire object —
    /// `StatusReport` drops remote fields this build doesn't model,
    /// and `status -c` passes the reply through verbatim, so the
    /// middleware carries both rather than make callers re-extract.
    /// (`report` boxed — a StatusReport is ~350 B and answers are
    /// per-probe, so one heap alloc beats a fat enum.)
    Report { report: Box<StatusReport>, raw: serde_json::Value },
    /// exec failed without conn evidence.
    Transport(anyhow::Error),
    /// exec failed AND burned a conn_fail bump (dead endpoint).
    ConnDown(anyhow::Error),
    /// Answered but not a v1 StatusReport — binary drift (a stale or
    /// skewed remote shunt), not a dead daemon.
    Drift(anyhow::Error),
}

/// `status --json` on a resolved queue: one exec, one extract, one
/// typed parse. Replaces the per-site hand-rolled
/// exec→parse→classify — including the double extract `status`
/// needed for its raw report field.
pub fn remote_status(t: &dyn Transport, queue: &ResolvedQueue) -> StatusAnswer {
    match wire_call(t, queue, &["status", "--json"], None) {
        WireCall::Reply(out) => match wire::extract(&out).and_then(|raw| Ok((raw.clone(), serde_json::from_value::<StatusReport>(raw)?))) {
            Ok((raw, report)) => StatusAnswer::Report { report: Box::new(report), raw },
            Err(e) => StatusAnswer::Drift(e),
        },
        WireCall::RemoteErr(e) => StatusAnswer::Transport(e),
        WireCall::ConnDown(e) => StatusAnswer::ConnDown(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::registry::tests::HOME_LOCK;
    use crate::server::registry::ClientEntry;
    use std::sync::Mutex;

    fn shunt_home() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("shunt-remote-test-{}", std::process::id()));
        std::env::set_var("SHUNT_HOME", &dir);
        dir
    }

    /// Delta is before-vs-after: a call that bumps mid-flight burns;
    /// a pre-existing streak with no new bump does not.
    #[test]
    fn conn_fail_delta_is_relative_not_absolute() {
        let _g = HOME_LOCK.lock().unwrap();
        let dir = shunt_home();
        let bump = registry::conn_fail_bump("box");
        assert!(bump >= 1);
        // A failing call that never bumps: not this call's outage.
        let (_r, burned) = conn_fail_delta("box", || -> Result<()> { anyhow::bail!("remote-side error") });
        assert!(!burned);
        // A call that bumps: burned, even though it then "recovered".
        let (r, burned) = conn_fail_delta("box", || -> Result<()> {
            registry::conn_fail_bump("box");
            Ok(())
        });
        assert!(r.is_ok() && burned);
        // An OK call that leaves the streak untouched: not burned.
        registry::conn_fail_reset("box");
        let (_r, burned) = conn_fail_delta("box", || -> Result<()> { Ok(()) });
        assert!(!burned);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Fake transport: `flap` bumps the streak before failing (ssh
    /// rc-255 behavior); `out` is canned stdout when the call
    /// "answers".
    struct Fake {
        out: &'static str,
        flap: bool,
        calls: Mutex<u32>,
    }

    impl Transport for Fake {
        fn exec(&self, client: &ClientEntry, _argv: &[String], _stdin: Option<&str>) -> Result<String> {
            *self.calls.lock().unwrap() += 1;
            if self.flap {
                registry::conn_fail_bump(&client.name);
                anyhow::bail!("unreachable rc=255");
            }
            if self.out.is_empty() {
                anyhow::bail!("remote exec failed: queue wedged");
            }
            Ok(self.out.to_string())
        }
        fn copy_to(&self, _c: &ClientEntry, _l: &std::path::Path, _r: &str) -> Result<()> {
            Ok(())
        }
    }

    fn queue(name: &str) -> ResolvedQueue {
        let mut client = crate::server::client_ops::test_entry();
        client.name = name.into();
        ResolvedQueue { name: "q".into(), workers: None, client }
    }

    /// wire_call classifies: reply, remote-side error, and a
    /// conn-burned failure — each lands in its own variant.
    #[test]
    fn wire_call_classifies_all_three() {
        let _g = HOME_LOCK.lock().unwrap();
        let dir = shunt_home();
        let q = queue("box");
        let ok = Fake { out: "{}", flap: false, calls: Mutex::new(0) };
        assert!(matches!(wire_call(&ok, &q, &["status"], None), WireCall::Reply(_)));
        let wedged = Fake { out: "", flap: false, calls: Mutex::new(0) };
        assert!(matches!(wire_call(&wedged, &q, &["status"], None), WireCall::RemoteErr(_)));
        let dead = Fake { out: "", flap: true, calls: Mutex::new(0) };
        assert!(matches!(wire_call(&dead, &q, &["status"], None), WireCall::ConnDown(_)));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// remote_status: a good report parses AND carries the raw wire
    /// object; drift and conn-down stay distinct failures.
    #[test]
    fn remote_status_split() {
        let _g = HOME_LOCK.lock().unwrap();
        let dir = shunt_home();
        let q = queue("box");
        // Serialize a real StatusReport (required fields carry no
        // serde defaults), then inject a field the type doesn't
        // model to prove `raw` passes it through.
        let mut rep = serde_json::to_value(&StatusReport { protocol: wire::PROTOCOL_VERSION, daemon: crate::proto::wire::DaemonState { alive: true, pid: Some(7), ..Default::default() }, ..Default::default() }).unwrap();
        rep["extra_remote_field"] = serde_json::json!([1]);
        let s = rep.to_string();
        let good = Fake { out: Box::leak(s.into_boxed_str()), flap: false, calls: Mutex::new(0) };
        match remote_status(&good, &q) {
            StatusAnswer::Report { report, raw } => {
                assert!(report.daemon.alive && report.daemon.pid == Some(7));
                // raw keeps fields StatusReport doesn't model.
                assert!(raw.get("extra_remote_field").is_some());
            }
            other => panic!("expected Report, got {}", std::any::type_name_of_val(&other)),
        }
        let drift = Fake { out: "banner\n{\"ok\":true}\n", flap: false, calls: Mutex::new(0) };
        assert!(matches!(remote_status(&drift, &q), StatusAnswer::Drift(_)));
        let dead = Fake { out: "", flap: true, calls: Mutex::new(0) };
        assert!(matches!(remote_status(&dead, &q), StatusAnswer::ConnDown(_)));
        let wedged = Fake { out: "", flap: false, calls: Mutex::new(0) };
        assert!(matches!(remote_status(&wedged, &q), StatusAnswer::Transport(_)));
        std::fs::remove_dir_all(&dir).ok();
    }
}
