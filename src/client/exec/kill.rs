//! Process-group signaling: the §3.9a ownership gates, TERM→KILL
//! escalation, and honest kill outcomes.

use anyhow::{Context, Result};
use std::path::Path;

use crate::client::queue;

/// Outcome of `signal_pgid`. The distinction matters: `Gone`
/// (ESRCH) is the ONLY proof the group does not exist — an EPERM or
/// any other errno means the signal could not be sent but the group
/// may be alive, so collapsing it to `Gone` would lie.
pub enum SignalOutcome {
    /// kill(2) succeeded; the signal was delivered to ≥1 member.
    Delivered,
    /// ESRCH: no such process group — provably absent.
    Gone,
    /// EPERM or another errno: cannot signal, group may exist.
    Failed(std::io::Error),
}

/// Send a signal to a whole process group (`kill(-pgid, sig)`).
/// Signals go through libc directly, not an external `kill` binary:
/// slim containers may lack it, and a failed exec must never read
/// as "group is gone".
pub fn signal_pgid(pgid: u32, sig: &str) -> SignalOutcome {
    let num = match sig {
        "0" => 0,
        "HUP" | "SIGHUP" => libc::SIGHUP,
        "INT" | "SIGINT" => libc::SIGINT,
        "TERM" | "SIGTERM" => libc::SIGTERM,
        "KILL" | "SIGKILL" => libc::SIGKILL,
        other => other.trim_start_matches("SIG").parse::<i32>().unwrap_or(-1),
    };
    // Valid signal range: 0..64 covers every standard and realtime
    // signal on Linux (NSIG is 65); a bogus name never reaches kill.
    if !(0..64).contains(&num) {
        return SignalOutcome::Failed(std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("unknown signal {sig}")));
    }
    // pgid 0/1 are never task groups — a spawned leader is a real
    // pid — and they are catastrophically special to kill(2):
    // `kill(0, sig)` signals the CALLER's own process group (the
    // daemon's / ssh session's) and `kill(-1, sig)` broadcasts to
    // every process the uid can reach. A corrupted pid file must not
    // turn `shunt kill` into a self-kill (design.md §3.9a).
    if pgid < 2 {
        return SignalOutcome::Failed(std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("refusing to signal pgid {pgid}")));
    }
    // Negative pid targets the whole process group |pgid|.
    let rc = unsafe { libc::kill(-(pgid as i32), num) };
    if rc == 0 {
        return SignalOutcome::Delivered;
    }
    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::ESRCH) => SignalOutcome::Gone,
        _ => SignalOutcome::Failed(err),
    }
}

/// Is any member of process group `pgid` still alive? `kill -0`
/// semantics: a successful probe OR EPERM (group exists but is owned
/// by another uid) both mean the group is there; only ESRCH means
/// gone.
pub fn pgid_alive(pgid: u32) -> bool {
    match signal_pgid(pgid, "0") {
        SignalOutcome::Delivered => true,
        SignalOutcome::Failed(e) => e.raw_os_error() == Some(libc::EPERM),
        SignalOutcome::Gone => false,
    }
}

/// Outcome of `kill_task`. `Gone` means the pid file's pgid no longer
/// names this task's leader — reaped by the daemon already, or the
/// pid was recycled by the OS — so no signal was ever sent. The kill
/// command reports this honestly instead of claiming a kill.
#[derive(Debug)]
pub enum KillOutcome {
    Signaled { pgid: u32, escalated: bool },
    Gone,
}

/// Parse a running entry's `pid` file: the leader pid (= task pgid)
/// optionally followed by the leader's /proc start_time recorded at
/// spawn. The start-time pin is the TOCTOU guard against pgid reuse
/// between reading the file and sending the signal.
pub fn read_pid_file(root: &Path, track: &str, name: &str) -> Option<(u32, Option<u64>)> {
    let s = std::fs::read_to_string(queue::piece_file(root, track, name, "pid")).ok()?;
    let mut it = s.split_whitespace();
    // pgid < 2 parses but is never a valid task pgid (0/1 are kill(2)
    // specials — see signal_pgid); treat it as unparseable so every
    // downstream gate sees "no pid", not a group to probe.
    let pgid: u32 = it.next()?.parse().ok().filter(|&p| p >= 2)?;
    let start = it.next().and_then(|t| t.parse().ok());
    Some((pgid, start))
}

/// `/proc/<pid>/stat` → `(pgrp, start_time)`. comm is skipped via the
/// last `)` so a parenthesised/spaced comm cannot shift the fields
/// (fields after comm: state=f[0], ppid=f[1], pgrp=f[2], …,
/// start_time=f[19]).
pub fn proc_pgrp_start(pid: u32) -> Option<(u32, u64)> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = s.rsplit_once(')')?.1;
    let f: Vec<&str> = rest.split_whitespace().collect();
    Some((f.get(2)?.parse().ok()?, f.get(19)?.parse().ok()?))
}

/// The pid file's pgid still names this task's leader: the leader pid
/// is live, is its own process-group leader, and — when a start_time
/// was recorded — is the same process the daemon spawned, not a
/// recycled pid. Signals only ever go out when this holds
/// (design.md §3.1: never signal anything that isn't ours).
pub fn pgid_is_task_leader(pgid: u32, recorded_start: Option<u64>) -> bool {
    match proc_pgrp_start(pgid) {
        Some((pgrp, start)) => pgrp == pgid && recorded_start.is_none_or(|s| s == start),
        None => false,
    }
}

/// Signal gate (design.md §3.9a): the recorded pgid provably still
/// names this task's group when EITHER the leader check above holds,
/// OR the leader pid is dead while the group still holds members — a
/// pgid cannot be recycled while its group is live, so a live remnant
/// is necessarily this task's own leftover group (detached
/// grandchildren from `cmd &` survive a leader that exits on TERM).
/// Only a fully dead group fails the gate: there the pgid may have
/// been recycled to a stranger, and signaling must not happen. A live
/// pid at `pgid` that fails the leader check is the recycled case
/// (foreign leader took the pgid) — also refused.
pub fn pgid_is_task_group(pgid: u32, recorded_start: Option<u64>) -> bool {
    // Belt at the gate, same bound as signal_pgid: pgid 0 would pass
    // the dead-leader branch (proc_pgrp_start(0) fails but
    // pgid_alive(0) probes our own group and succeeds), and pgid 1
    // can pass the leader check in containers where init's pgrp is 1
    // — neither may ever authorize a signal (design.md §3.9a).
    pgid >= 2 && (pgid_is_task_leader(pgid, recorded_start) || (proc_pgrp_start(pgid).is_none() && pgid_alive(pgid)))
}

/// TERM → 10×1 s → KILL on the task's recorded pgid. The pgid comes
/// only from the entry's `pid` file, never from a name match, and
/// every signal passes the §3.9a group gate: leader-verified, or
/// leader dead but the group still live (a live pgid is unrecyclable,
/// so the remnant is provably ours). A fully dead group — pgid free
/// to be recycled — reports `Gone` and is never signaled.
pub fn kill_task(root: &Path, task_name: &str) -> Result<KillOutcome> {
    kill_task_with(root, task_name, signal_pgid, std::thread::sleep)
}

/// `kill_task` with the signal sender and the drain-loop sleep
/// swapped out — tests inject both to reach the ESRCH races that a
/// real kill(2) can only hit by luck (group dying between the gate
/// and the send).
pub(super) fn kill_task_with(root: &Path, task_name: &str, sig: fn(u32, &str) -> SignalOutcome, nap: fn(std::time::Duration)) -> Result<KillOutcome> {
    let (pgid, start) = read_pid_file(root, "running", task_name).with_context(|| format!("no pid file for {task_name}"))?;
    if !pgid_is_task_group(pgid, start) {
        return Ok(KillOutcome::Gone);
    }
    // A TERM that cannot be sent (EPERM, bad arg) is an error, not
    // a silent Signaled — the kill command must not claim a signal
    // it never delivered (design.md §3.9a). An ESRCH here means the
    // group died between the gate and the send: nothing was ever
    // signaled, so the honest result is Gone, not Signaled.
    match sig(pgid, "TERM") {
        SignalOutcome::Delivered => {}
        SignalOutcome::Gone => return Ok(KillOutcome::Gone),
        SignalOutcome::Failed(e) => return Err(e).with_context(|| format!("cannot TERM pgid {pgid}")),
    }
    for _ in 0..10 {
        if !pgid_alive(pgid) {
            return Ok(KillOutcome::Signaled { pgid, escalated: false });
        }
        nap(std::time::Duration::from_secs(1));
    }
    // Re-verify before escalating (§3.9a): a leader that exited on
    // TERM while its group lives on still gets KILL — the live pgid
    // cannot name a stranger. Only a fully drained group is skipped.
    if !pgid_is_task_group(pgid, start) {
        return Ok(KillOutcome::Signaled { pgid, escalated: false });
    }
    match sig(pgid, "KILL") {
        SignalOutcome::Delivered => Ok(KillOutcome::Signaled { pgid, escalated: true }),
        // ESRCH at the KILL send means the group died post-TERM:
        // TERM did land but no KILL was ever sent, so `escalated`
        // must not claim one (§3.9a honesty).
        SignalOutcome::Gone => Ok(KillOutcome::Signaled { pgid, escalated: false }),
        SignalOutcome::Failed(e) => Err(e).with_context(|| format!("cannot KILL pgid {pgid}")),
    }
}
