//! Task spawn/finalize (design.md §3.7, §3.9, §3.12): process_group(0)
//! so pgid = leader pid, log header/trailer, done/failed split.

use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use crate::agent::queue;

/// Proxy vars every task loses; a task that needs a tunnel re-exports
/// it inside itself (design.md §3.12).
const PROXY_VARS: [&str; 6] = ["ALL_PROXY", "all_proxy", "http_proxy", "https_proxy", "HTTP_PROXY", "HTTPS_PROXY"];

/// Read the dotenv-style `env` file under the queue root
/// (`KEY=value` per line, `#` comments and blanks skipped). The
/// runner consumes `WORKDIR`, `PATH_PREPEND` and `TASK_THREADS` itself
/// and injects any remaining keys into the task's environment.
///
/// Values are LITERAL: there is no shell between the file and
/// `Command::env`, so no quote stripping, no `$VAR` expansion, no
/// escape processing. `K="a b"` yields a value that *contains* the
/// quote characters; write `K=a b` instead — embedded spaces are
/// preserved verbatim. The first `=` splits key/value; the KEY is
/// whitespace-trimmed (keys can't contain spaces anyway) but the
/// value is kept strictly literal — `K= padded ` passes the spaces
/// through to `Command::env` untouched.
pub fn read_env_file(root: &Path) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    if let Ok(text) = std::fs::read_to_string(root.join("env")) {
        for line in text.lines() {
            // trim_start only: leading whitespace may indent a `#`
            // comment or blank line, but a trim_end would eat
            // meaningful trailing spaces in the value (§3.12 literal).
            let line = line.trim_start();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                let k = k.trim();
                // `Command::env` panics on an empty key or on NUL in
                // key/value — a `=foo` or NUL-bearing line must be
                // dropped here, not crash the daemon mid-dispatch
                // (a claimed entry that panics dispatch requeues as
                // an orphan and crash-loops forever, design.md §3.6).
                if !k.is_empty() && !k.contains('\0') && !v.contains('\0') {
                    map.insert(k.to_string(), v.to_string());
                }
            }
        }
    }
    map
}

/// Spawn a claimed task under its own process group (pgid = leader
/// pid), output appended to `running/<name>.log`. The `.pid` file is
/// written by the daemon right after this returns. The returned
/// `TaskDirectives` is the task's parsed comment head — the daemon
/// counts `timeout_s` from spawn (a daemon restart re-times the
/// requeued rerun wholesale, design.md §3.6) and reads the
/// reschedule budget from `retry` at reap time (§3.9).
pub fn spawn_task(root: &Path, task_name: &str) -> Result<(Child, TaskDirectives)> {
    let task = queue::task_file(root, "running", task_name);
    let log_path = queue::piece_file(root, "running", task_name, "log");

    {
        let mut log = std::fs::OpenOptions::new().create(true).write(true).truncate(true).open(&log_path)?;
        writeln!(log, "=== task {task_name}")?;
        writeln!(log, "=== start {}", crate::proto::time::rfc3339_offset_now())?;
    }

    let env = read_env_file(root);
    let td = task_directives(&task);
    let workdir = td.workdir.clone().or_else(|| env.get("WORKDIR").cloned()).unwrap_or_else(|| root.to_string_lossy().into_owned());

    let log_out = std::fs::OpenOptions::new().create(true).append(true).open(&log_path)?;
    let log_err = log_out.try_clone()?;

    // Task shell: `<qroot>/shell` if the client pushed one, else
    // bash. env_setup is a shell snippet sourced in the wrapper
    // before exec'ing the task (design.md §3.12) — conda activate /
    // `source env.sh` work because env mutations survive exec; a
    // missing or erroring file never blocks the task.
    // A NUL-bearing shell path would panic `Command::new`/`arg` —
    // treat it like a missing file and fall back to bash rather than
    // crash dispatch.
    let shell = std::fs::read_to_string(root.join("shell")).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty() && !s.contains('\0')).unwrap_or_else(|| "bash".into());
    let mut cmd = Command::new(&shell);
    cmd.arg("-c")
        // rc 97 when the workdir is unreachable (design.md §3.12)
        .arg("cd \"$1\" || { echo \"shunt: cannot cd to $1\" >&2; exit 97; }; [ -f \"$2/env_setup\" ] && . \"$2/env_setup\"; exec \"$0\" \"$3\"")
        .arg(&shell)
        .arg(&workdir)
        .arg(root)
        .arg(&task)
        .stdin(Stdio::null())
        .stdout(log_out)
        .stderr(log_err)
        .process_group(0);
    for v in PROXY_VARS {
        cmd.env_remove(v);
    }
    if let Some(threads) = env.get("TASK_THREADS") {
        // Values are literal (§3.12), so exact `auto` is the norm;
        // the trim only rescues padded whitespace like `auto `.
        let n = if threads.trim() == "auto" {
            crate::agent::probe::cgroup_cores().unwrap_or(1)
        } else {
            match queue::digits_only(threads) {
                Some(n) => n,
                // Fail loud, not silently capped: a value with no
                // digits (`x`, `half`) carries no number at all, so
                // capping at 1 without a word would be silent
                // degradation. Still cap at 1 — unlimited threads
                // thrash a cgroup-capped box — but say so in the log.
                None => {
                    if let Ok(mut log) = std::fs::OpenOptions::new().append(true).open(&log_path) {
                        let _ = writeln!(log, "=== warn: TASK_THREADS={threads:?} has no digits; capping threads to 1");
                    }
                    1
                }
            }
        };
        let n = n.max(1).to_string();
        cmd.env("OMP_NUM_THREADS", &n);
        cmd.env("MKL_NUM_THREADS", &n);
    }
    for (k, v) in &env {
        if !matches!(k.as_str(), "WORKDIR" | "PATH_PREPEND" | "TASK_THREADS") {
            cmd.env(k, v);
        }
    }
    // PATH_PREPEND applies AFTER the generic env passthrough: a literal
    // `PATH=` line in the env file is the base the prepend extends, not
    // an override that silently erases it — effective PATH is always
    // `PATH_PREPEND:PATH` (env-file PATH if present, else the daemon's),
    // per design.md §3.12.
    if let Some(prepend) = env.get("PATH_PREPEND") {
        let cur = env.get("PATH").cloned().or_else(|| std::env::var("PATH").ok()).unwrap_or_default();
        cmd.env("PATH", format!("{prepend}:{cur}"));
    }
    // Per-task `shunt-env:` directives applied last: a submitted
    // binding's overlay beats the queue-level env file.
    for (k, v) in &td.envs {
        cmd.env(k, v);
    }
    cmd.spawn().map(|c| (c, td)).context("spawn task failed")
}

/// Directives parsed from a task file's comment head (design.md
/// §3.13). Carried in the daemon's runpid handle so reap can reach
/// the retry budget without re-reading the file.
#[derive(Debug, Default)]
pub struct TaskDirectives {
    pub workdir: Option<String>,
    pub envs: Vec<(String, String)>,
    pub timeout_s: Option<u64>,
    pub retry: Option<u32>,
    pub retry_delay_s: Option<u64>,
    pub gpu_mem_mb: Option<u32>,
    pub ram_mb: Option<u32>,
    pub cpu_cores: Option<u32>,
}

/// Parse the task file's comment head for per-task overrides
/// (design.md §3.13):
///   `# shunt:workdir=<abs path>`   — runner cwd for this task only
///   `# shunt-env:<KEY>=<value>`    — env overlay, beats queue `env`
///   `# shunt:timeout_s=<secs>`     — wall-clock budget; 0 = unlimited
///   `# shunt:retry=<n>`            — reschedule budget on rc 75 (§3.9)
///   `# shunt:retry_delay_s=<secs>` — wait before a requeued task is claimable
///   `# shunt:gpu_mem_mb|ram_mb|cpu_cores=<n>` — declared estimates, annotation only (§3.16)
/// Directives live in the comment head only; scanning stops at the
/// first non-comment, non-blank line. Unknown `# shunt:` lines are
/// skipped silently, so a task pushed to an older runner degrades to
/// a no-op comment (backward compat).
pub fn task_directives(task: &Path) -> TaskDirectives {
    let mut td = TaskDirectives::default();
    let Ok(text) = std::fs::read_to_string(task) else {
        return td;
    };
    for line in text.lines() {
        // trim_start like read_env_file: an indented `#` is still a
        // comment to the shell, so leading whitespace must neither
        // end the head scan nor hide a directive (§3.13's "first
        // non-comment line"). Value literals are unaffected — the
        // trim only touches bytes before `#`.
        let line = line.trim_start();
        if let Some(w) = line.strip_prefix("# shunt:workdir=") {
            // Value is literal like the env file (§3.12) — no trim.
            // NUL would panic `Command::arg` at spawn — drop the
            // directive so the env-file/default workdir applies.
            if !w.contains('\0') {
                td.workdir = Some(w.to_string());
            }
        } else if let Some(t) = line.strip_prefix("# shunt:timeout_s=") {
            // Number parse stays whitespace-tolerant: `3600 ` means
            // 3600 either way, unlike env values where spaces are data.
            td.timeout_s = t.trim().parse::<u64>().ok().filter(|&n| n > 0);
        } else if let Some(kv) = line.strip_prefix("# shunt-env:") {
            if let Some((k, v)) = kv.split_once('=') {
                // Key trimmed, value literal — the same semantics as
                // the env file for the two KEY=value surfaces.
                let k = k.trim();
                // Same panic guard as the env file: `# shunt-env:=x`
                // yields an empty key and a NUL in either half is a
                // `Command::env` panic — both must be dropped.
                if !k.is_empty() && !k.contains('\0') && !v.contains('\0') {
                    td.envs.push((k.to_string(), v.to_string()));
                }
            }
        } else if let Some(n) = line.strip_prefix("# shunt:retry=") {
            td.retry = n.trim().parse::<u32>().ok();
        } else if let Some(n) = line.strip_prefix("# shunt:retry_delay_s=") {
            td.retry_delay_s = n.trim().parse::<u64>().ok();
        } else if let Some(n) = line.strip_prefix("# shunt:gpu_mem_mb=") {
            td.gpu_mem_mb = n.trim().parse::<u32>().ok();
        } else if let Some(n) = line.strip_prefix("# shunt:ram_mb=") {
            td.ram_mb = n.trim().parse::<u32>().ok();
        } else if let Some(n) = line.strip_prefix("# shunt:cpu_cores=") {
            td.cpu_cores = n.trim().parse::<u32>().ok();
        } else if !line.starts_with('#') && !line.trim().is_empty() {
            break;
        }
    }
    td
}

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
fn kill_task_with(root: &Path, task_name: &str, sig: fn(u32, &str) -> SignalOutcome, nap: fn(std::time::Duration)) -> Result<KillOutcome> {
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

/// Quarantine: move a stuck entry to `failed/` with a `shunt_error`
/// note — the catch-all exit for entries that can neither run nor be
/// left invisible (claim collisions, failed finalize/requeue
/// renames). A same-name failed/ entry gets a timestamped suffix
/// rather than being clobbered.
pub fn quarantine(root: &Path, track: &str, name: &str, note: &str) -> Result<()> {
    // Nothing to quarantine is a no-op, not an error: when the source
    // entry is already gone (e.g. finalize's rename succeeded and only
    // the exit-file write failed) the real entry sits in done/ — a
    // `failed/<name>/` holding only `shunt_error` would report a task
    // as failed that actually completed.
    let src_dir = root.join(track).join(name);
    if !src_dir.exists() && !root.join(track).join(format!("{name}.sh")).exists() {
        return Ok(());
    }
    let failed = root.join("failed");
    std::fs::create_dir_all(&failed)?;
    let mut dest = failed.join(name);
    if dest.exists() || failed.join(format!("{name}.sh")).exists() {
        dest = failed.join(format!("{name}.quarantined.{}", crate::proto::time::unix_now()));
    }
    if src_dir.is_dir() {
        std::fs::rename(&src_dir, &dest).with_context(|| format!("cannot quarantine {name}"))?;
    } else {
        std::fs::create_dir_all(&dest)?;
        let flat = root.join(track).join(format!("{name}.sh"));
        if flat.exists() {
            std::fs::rename(&flat, dest.join("task.sh")).with_context(|| format!("cannot quarantine {name}"))?;
        }
    }
    let _ = std::fs::write(dest.join("shunt_error"), format!("{note}\n"));
    Ok(())
}

/// Finalize a reaped task: `=== exit` trailer, `exit` file, move the
/// entry dir to done/ (rc 0) or failed/. Legacy flat entries are
/// normalized into the dir layout here, so old queues converge.
/// `timeout_s`: the task exceeded its wall-clock budget and was
/// killed by the daemon — the exit file keeps the observed signal rc
/// (143 TERM / 137 KILL) and a `timeout` marker file records the
/// exceeded limit so agents can distinguish budget death from a
/// plain non-zero rc.
pub fn finalize(root: &Path, base: &str, rc: i32, timeout_s: Option<u64>) -> Result<()> {
    let running = root.join("running");
    // A timed-out task always lands in failed/, even if its last
    // action was a clean exit 0 caught under the TERM.
    let dest_track = root.join(if rc == 0 && timeout_s.is_none() { "done" } else { "failed" });
    std::fs::create_dir_all(&dest_track)?;
    let dest = dest_track.join(base);
    if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(queue::piece_file(root, "running", base, "log")) {
        if let Some(limit) = timeout_s {
            let _ = writeln!(f, "=== timeout: exceeded wall-clock limit {limit}s");
        }
        let _ = writeln!(f, "=== exit {rc} {}", crate::proto::time::rfc3339_offset_now());
    }
    if queue::is_dir_entry(root, "running", base) {
        // Write `exit`/`timeout` INSIDE the entry dir before the
        // rename — not into `dest` after it, as the spec's literal
        // order suggests (design.md §3.9). The sidecars travel with
        // the dir so success is identical, but a write failure now
        // leaves the entry in running/ where the caller's quarantine
        // fallback can still move it to failed/ with a `shunt_error`
        // note. Writing post-rename would let a failed `exit` write
        // strand an exit-less terminal entry: `recent_exits` filters
        // on `exit` existing, so the completed task reports
        // `unknown` forever (status + lazy `task show`).
        let entry = running.join(base);
        std::fs::write(entry.join("exit"), format!("{rc}\n"))?;
        if let Some(limit) = timeout_s {
            let _ = std::fs::write(entry.join("timeout"), format!("{limit}\n"));
        }
        let _ = std::fs::remove_file(entry.join("pid"));
        std::fs::rename(&entry, &dest).with_context(|| format!("cannot move {base} to {}", dest.display()))?;
    } else {
        // Legacy flat entry: no source dir exists to pre-write into,
        // so `exit` lands in `dest` before the pieces move — a failed
        // task.sh rename then leaves dest holding `exit` (+ the
        // caller's quarantine still applies to the flat remainder),
        // never a terminal entry without an exit marker.
        let dest_preexisting = dest.exists();
        std::fs::create_dir_all(&dest)?;
        std::fs::write(dest.join("exit"), format!("{rc}\n"))?;
        if let Some(limit) = timeout_s {
            let _ = std::fs::write(dest.join("timeout"), format!("{limit}\n"));
        }
        if let Err(e) = std::fs::rename(running.join(format!("{base}.sh")), dest.join("task.sh")) {
            // A `dest` we just created holds ONLY `exit` (+`timeout`):
            // left behind it reports the task as done/failed via
            // `list_entries` + `recent_exits` while the caller's
            // quarantine also lands the flat remainder in failed/ —
            // a phantom double-outcome. Remove it, but only when this
            // call created it: a pre-existing `dest` may be a real
            // older same-named entry whose data `remove_dir_all`
            // would destroy.
            if !dest_preexisting {
                let _ = std::fs::remove_dir_all(&dest);
            }
            return Err(e).with_context(|| format!("cannot move {base} to {}", dest.display()));
        }
        let log = running.join(format!("{base}.sh.log"));
        if log.exists() {
            std::fs::rename(&log, dest.join("log"))?;
        }
        let _ = std::fs::remove_file(running.join(format!("{base}.sh.pid")));
    }
    // Terminal tracks are unbounded without a sweep: past the keep
    // cap the oldest entries move to `<track>/.archive/` so `status`
    // scans stay cheap on long-lived queues (design.md §3.9).
    let track_name = if rc == 0 && timeout_s.is_none() { "done" } else { "failed" };
    queue::archive_oldest(root, track_name, queue::track_keep());
    Ok(())
}

/// Default wait before a rescheduled task may be claimed again when
/// the task doesn't declare `# shunt:retry_delay_s=` (design.md
/// §3.9). Long enough that a transient co-tenant burst can clear;
/// short enough that a paid box doesn't idle for a wrong guess.
pub const DEFAULT_RETRY_DELAY_S: u64 = 60;

/// What `maybe_reschedule` decided — `Queued` reports the requeue
/// details for the daemon's `task_rescheduled` event.
pub enum Resched {
    /// Not eligible: no retry budget declared, budget exhausted,
    /// the rc isn't reschedule-shaped, or a same-named pending
    /// entry already holds the target.
    No,
    /// Moved back to `pending/` under its original name.
    Queued { attempt: u32, via: &'static str, next_at: u64 },
}

/// Bounded OOM-signature scan over the tail of a running task's
/// log — the heuristic half of reschedule eligibility (design.md
/// §3.9): only tasks that declared `# shunt:retry=` ever reach it,
/// so a misread costs at most a bounded rerun, and a task that
/// reports rc 75 itself never needs the scan at all.
fn log_says_oom(root: &Path, base: &str) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let log = queue::piece_file(root, "running", base, "log");
    let Ok(mut f) = std::fs::File::open(&log) else {
        return false;
    };
    // OOM tracebacks sit at the end of the log; 16 KiB covers the
    // longest plausible traceback block.
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let _ = f.seek(SeekFrom::Start(len.saturating_sub(16 * 1024)));
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return false;
    }
    let hay = String::from_utf8_lossy(&buf).to_lowercase();
    ["out of memory", "outofmemoryerror", "cuda_error_out_of_memory", "alloc_failed", "cannot allocate memory"].iter().any(|p| hay.contains(p))
}

/// Reschedule a failed task instead of finalizing it (design.md
/// §3.9): the task opts in with `# shunt:retry=N`, reports
/// "resource was busy" via rc 75 (EX_TEMPFAIL), or — because most
/// real OOM exits are plain rc 1 — trips the log heuristic. The
/// entry keeps its `NNNN_` name: the number stays leased to this
/// task, `task show`'s lazy queue_name match keeps resolving, and
/// it rejoins pending/ at its sorted position. The failed
/// attempt's log is swept to `failed/<base>.attempt<K>.log` — spawn
/// truncates `log` each run, so keeping it inside would destroy
/// the evidence. `attempts`/`retry_after` sidecars inside the
/// entry carry the consumed budget and the wait deadline.
/// Returns `Resched::No` when the task should just finalize
/// normally; `Err` means the requeue itself broke (the caller
/// finalizes anyway and logs the failure loudly).
pub fn maybe_reschedule(root: &Path, base: &str, rc: i32, td: &TaskDirectives) -> Result<Resched> {
    let Some(max_retry) = td.retry.filter(|&n| n > 0) else {
        return Ok(Resched::No);
    };
    let entry = root.join("running").join(base);
    if !entry.is_dir() {
        return Ok(Resched::No); // legacy flat entry: no sidecar home
    }
    let attempts: u32 = std::fs::read_to_string(entry.join("attempts")).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    if attempts >= max_retry {
        return Ok(Resched::No); // budget exhausted -> failed/
    }
    let via = if rc == 75 {
        "rc75"
    } else if log_says_oom(root, base) {
        "oom_heuristic"
    } else {
        return Ok(Resched::No);
    };
    // A backward move can evade enqueue's pending -> running scan.
    // Serialize the collision check and mutation with that scan; this
    // local guard drops on every return, before finalize or another spawn.
    let _lock = queue::seq_lock(root)?;
    let pending = root.join("pending");
    if pending.join(base).exists() || pending.join(format!("{base}.sh")).exists() {
        // Same-numbered pending entry already exists — requeueing
        // would collide. Finalize normally; the caller logs the
        // finish line, and the anomaly is visible in failed/.
        return Ok(Resched::No);
    }
    // Sidecars go in BEFORE the move so a failed write bails while
    // the entry still sits in running/ — the caller then finalizes
    // it normally rather than losing the budget bookkeeping.
    let attempt = attempts + 1;
    std::fs::write(entry.join("attempts"), format!("{attempt}\n"))?;
    let next_at = crate::proto::time::unix_now() + td.retry_delay_s.unwrap_or(DEFAULT_RETRY_DELAY_S);
    std::fs::write(entry.join("retry_after"), format!("{next_at}\n"))?;
    let _ = std::fs::remove_file(entry.join("pid"));
    std::fs::create_dir_all(root.join("failed"))?;
    if entry.join("log").exists() {
        let _ = std::fs::rename(entry.join("log"), root.join("failed").join(format!("{base}.attempt{attempt}.log")));
    }
    std::fs::rename(&entry, pending.join(base)).with_context(|| format!("cannot requeue {base} into pending/"))?;
    Ok(Resched::Queued { attempt, via, next_at })
}

/// Exit status → shell-style rc: exit code, or 128+signal.
pub fn status_rc(status: &std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    if let Some(code) = status.code() {
        code
    } else if let Some(sig) = status.signal() {
        128 + sig
    } else {
        1
    }
}

/// Render a task body from metadata — the single generator for
/// `new`/`mktask` and server-side `submit` (design.md §3.13–3.14).
/// No `cd` line: the runner places cwd at the binding's workdir.
pub fn render_task(meta: &crate::proto::task::TaskMeta) -> String {
    let mut out = String::from("#!/usr/bin/env bash\n");
    out.push_str(&format!("# task: {} (generated by shunt {})\n", meta.slug, crate::proto::time::rfc3339_offset_now()));
    // Per-task directives ride in the comment head; exec.rs applies
    // them at spawn (the wire carries only the body). `id` is the
    // exception: the enqueue allocator consumes it for delivery
    // dedup (design.md §3.3), the runner never looks at it.
    if let Some(i) = &meta.id {
        out.push_str(&format!("# shunt:id={i}\n"));
    }
    if let Some(w) = &meta.workdir {
        out.push_str(&format!("# shunt:workdir={w}\n"));
    }
    if let Some(t) = meta.timeout_s.filter(|&n| n > 0) {
        out.push_str(&format!("# shunt:timeout_s={t}\n"));
    }
    if let Some(n) = meta.retry.filter(|&n| n > 0) {
        out.push_str(&format!("# shunt:retry={n}\n"));
    }
    if let Some(n) = meta.retry_delay_s {
        out.push_str(&format!("# shunt:retry_delay_s={n}\n"));
    }
    if let Some(n) = meta.gpu_mem_mb {
        out.push_str(&format!("# shunt:gpu_mem_mb={n}\n"));
    }
    if let Some(n) = meta.ram_mb {
        out.push_str(&format!("# shunt:ram_mb={n}\n"));
    }
    if let Some(n) = meta.cpu_cores {
        out.push_str(&format!("# shunt:cpu_cores={n}\n"));
    }
    for (k, v) in &meta.env {
        out.push_str(&format!("# shunt-env:{k}={v}\n"));
    }
    out.push_str("set -uo pipefail\n");
    if let Some(guard) = &meta.guard {
        let q = crate::server::transport::shell_quote(guard);
        // printf %s, not a double-quoted echo: inside `"..."` the
        // single quotes around {q} are literal characters and a
        // hostile guard's `$( )`/backticks would still expand.
        out.push_str(&format!("if [ -e {q} ]; then printf 'guard hit: %s exists; skipping\\n' {q}; exit 0; fi\n"));
    }
    let cmd = meta.cmd.iter().map(|a| crate::server::transport::shell_quote(a)).collect::<Vec<_>>().join(" ");
    out.push_str(&format!("exec {cmd}\n"));
    out
}

/// Convenience: the path of a running task's log (`running/<name>/log`).
pub fn running_log(root: &Path, base: &str) -> PathBuf {
    queue::piece_file(root, "running", base, "log")
}

#[cfg(test)]
mod tests {
    use super::read_env_file;

    /// pgid 0/1 in a `pid` file are kill(2) specials, never task
    /// groups: `kill(0, sig)` would signal the daemon's own process
    /// group and `kill(-1, sig)` would broadcast to every signalable
    /// process. `read_pid_file` must reject them, the §3.9a gate must
    /// refuse them even when probed directly (pgid_alive(0) succeeds
    /// on our own group, and init's pgrp is 1 in containers), and
    /// `kill_task` must never reach a signal for either. Asserting on
    /// the gate predicate only — no kill(2) is ever issued here.
    #[test]
    fn kill_special_pgids_are_never_signaled() {
        for pgid in [0u32, 1] {
            assert!(!super::pgid_is_task_group(pgid, None), "pgid {pgid} passed the signal gate");
            match super::signal_pgid(pgid, "TERM") {
                super::SignalOutcome::Failed(e) => assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput),
                _ => panic!("signal_pgid({pgid}) did not refuse"),
            }

            let root = std::env::temp_dir().join(format!("shunt-badpgid-{pgid}-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
            let entry = root.join("running").join("t");
            std::fs::create_dir_all(&entry).unwrap();
            std::fs::write(entry.join("task.sh"), "exit 0\n").unwrap();
            std::fs::write(entry.join("pid"), format!("{pgid}\n")).unwrap();
            assert_eq!(super::read_pid_file(&root, "running", "t"), None);
            // No pid parses -> "no pid file" error; a pgid that did
            // parse would have to come back Gone, never Signaled.
            match super::kill_task(&root, "t") {
                Err(_) | Ok(super::KillOutcome::Gone) => {}
                Ok(super::KillOutcome::Signaled { .. }) => panic!("kill_task signaled pgid {pgid}"),
            }
            std::fs::remove_dir_all(&root).ok();
        }
    }

    /// §3.9a ESRCH races (design.md "a kill result is honest"): a
    /// group that dies between the §3.9a gate and the TERM send was
    /// never signaled — `kill_task` must report `Gone`, not a phantom
    /// kill. A group that dies between the escalation re-check and
    /// the KILL send did take the TERM but no KILL — `escalated`
    /// must not claim a signal that never went out. Both races are
    /// unreachable deterministically through real kill(2), so the
    /// sender and the drain sleep are injected; the pid file points
    /// at a real live process group so the real gates run.
    #[test]
    fn kill_esrch_races_report_honestly() {
        use std::os::unix::process::CommandExt;
        let mut child = std::process::Command::new("sleep").arg("30").process_group(0).spawn().unwrap();
        let pgid = child.id();
        let root = std::env::temp_dir().join(format!("shunt-esrch-{}-{}", std::process::id(), pgid));
        let entry = root.join("running").join("t");
        std::fs::create_dir_all(&entry).unwrap();
        std::fs::write(entry.join("task.sh"), "exit 0\n").unwrap();
        std::fs::write(entry.join("pid"), format!("{pgid}\n")).unwrap();

        // TERM hits ESRCH: nothing was sent -> Gone.
        match super::kill_task_with(&root, "t", |_, _| super::SignalOutcome::Gone, |_| {}) {
            Ok(super::KillOutcome::Gone) => {}
            other => panic!("TERM-ESRCH must report Gone, got {other:?}"),
        }

        // TERM delivered, group stays live through the drain, KILL
        // hits ESRCH: TERM landed but nothing escalated.
        let sig = |_: u32, s: &str| match s {
            "TERM" => super::SignalOutcome::Delivered,
            "KILL" => super::SignalOutcome::Gone,
            _ => unreachable!(),
        };
        match super::kill_task_with(&root, "t", sig, |_| {}) {
            Ok(super::KillOutcome::Signaled { pgid: p, escalated: false }) => assert_eq!(p, pgid),
            other => panic!("KILL-ESRCH must report Signaled{{escalated:false}}, got {other:?}"),
        }

        let _ = child.kill();
        let _ = child.wait();
        std::fs::remove_dir_all(&root).ok();
    }

    /// A hostile guard path must reach the guard line only as a
    /// single-quoted printf argv: inside a double-quoted message the
    /// quotes are literal characters and `$()`, backticks, or `"`
    /// would still expand when the guard-hit branch runs.
    #[test]
    fn guard_echo_uses_quoted_form() {
        for guard in ["runs/$(touch X).json", "runs/evil\".json", "runs/`id`.json"] {
            let meta = crate::proto::task::TaskMeta { id: None, slug: "t".into(), guard: Some(guard.into()), workdir: None, env: Default::default(), cmd: vec!["true".into()], timeout_s: None, retry: None, retry_delay_s: None, gpu_mem_mb: None, ram_mb: None, cpu_cores: None };
            let body = super::render_task(&meta);
            let q = crate::server::transport::shell_quote(guard);
            let line = format!("if [ -e {q} ]; then printf 'guard hit: %s exists; skipping\\n' {q}; exit 0; fi");
            assert!(body.contains(&line), "body missing quoted guard line for {guard:?}: {body}");
            // The quoted guard must never sit inside a double-quoted
            // string, where its `$()`/backticks would re-expand.
            assert!(!body.contains(&format!("\"guard hit: {q}")), "quoted guard inside a double-quoted echo for {guard:?}");
        }
    }

    /// Bash smoke check: run the rendered body with a live hostile
    /// guard and assert the `$( )` payload never executes — the
    /// message prints the guard literally.
    #[test]
    fn guard_hit_branch_cannot_expand() {
        let root = std::env::temp_dir().join(format!("shunt-guardhit-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let pwned = root.join("PWNED");
        // Relative guard name (bash runs with cwd=root) so the
        // `$( )` payload carries no slashes of its own.
        let guard = root.join("x$(touch PWNED).json");
        std::fs::write(&guard, "").unwrap();
        let meta = crate::proto::task::TaskMeta { id: None, slug: "t".into(), guard: Some("x$(touch PWNED).json".into()), workdir: None, env: Default::default(), cmd: vec!["true".into()], timeout_s: None, retry: None, retry_delay_s: None, gpu_mem_mb: None, ram_mb: None, cpu_cores: None };
        let body = super::render_task(&meta);
        let out = std::process::Command::new("bash").arg("-c").arg(&body).current_dir(&root).output().unwrap();
        assert!(out.status.success(), "bash exited {:?}: {}", out.status, String::from_utf8_lossy(&out.stderr));
        assert!(!pwned.exists(), "guard payload executed: {body}");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("guard hit: x$(touch PWNED).json exists; skipping"), "unexpected stdout: {stdout}");
        std::fs::remove_dir_all(&root).ok();
    }

    /// The env file is literal KEY=value (design.md §3.12): spaces
    /// survive verbatim — including leading/trailing ones in the
    /// value — but quotes are never stripped: `K="a b"` keeps the
    /// quote characters as part of the value.
    #[test]
    fn env_values_are_literal_no_quote_semantics() {
        let root = std::env::temp_dir().join(format!("shunt-envtest-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("env"), "# comment\n\nFOO=a b c\nQUOTED=\"a b\"\nSQUOTED='x'\nSPACED =  padded  \nNOEQ\n").unwrap();
        let m = read_env_file(&root);
        assert_eq!(m["FOO"], "a b c");
        assert_eq!(m["QUOTED"], "\"a b\"");
        assert_eq!(m["SQUOTED"], "'x'");
        // Value-side whitespace is data: only the key is trimmed.
        assert_eq!(m["SPACED"], "  padded  ");
        assert!(!m.contains_key("NOEQ"));
        std::fs::remove_dir_all(&root).ok();
    }

    /// `=foo` yields an empty key, and `Command::env` panics on it —
    /// the parse layer must drop it so the daemon never crashes
    /// mid-dispatch (same guard for `# shunt-env:=x` directives).
    #[test]
    fn empty_env_keys_are_dropped() {
        let root = std::env::temp_dir().join(format!("shunt-emptykey-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("env"), "=orphan-value\nGOOD=1\n  =also-empty\n").unwrap();
        let m = read_env_file(&root);
        assert_eq!(m.len(), 1);
        assert_eq!(m["GOOD"], "1");
        std::fs::remove_dir_all(&root).ok();

        let task = std::env::temp_dir().join(format!("shunt-emptydir-{}", std::process::id()));
        std::fs::write(&task, "#!/usr/bin/env bash\n# shunt-env:=bad\n# shunt-env:OK=yes\n# shunt-env:PAD= x \necho hi\n").unwrap();
        let td = super::task_directives(&task);
        let envs = td.envs;
        // Literal values here too: PAD's spaces ride through verbatim.
        assert_eq!(envs, vec![("OK".to_string(), "yes".to_string()), ("PAD".to_string(), " x ".to_string())]);
        std::fs::remove_file(&task).ok();
    }

    /// NUL is valid UTF-8, so `read_to_string` passes it through —
    /// but `Command::env`/`arg`/`new` panic on it. Every parsed
    /// surface must drop NUL-bearing entries the same way empty keys
    /// are dropped, or a hostile file crash-loops the daemon
    /// (panic in dispatch → orphan requeue → panic again).
    #[test]
    fn nul_entries_are_dropped_not_panics() {
        let root = std::env::temp_dir().join(format!("shunt-nul-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("env"), "BAD\u{0}KEY=v\nK2=v\u{0}x\nGOOD=1\n").unwrap();
        let m = read_env_file(&root);
        assert_eq!(m.len(), 1);
        assert_eq!(m["GOOD"], "1");

        let task = root.join("t.sh");
        std::fs::write(&task, "#!/usr/bin/env bash\n# shunt:workdir=/x\u{0}y\n# shunt-env:A\u{0}B=x\n# shunt-env:C=d\u{0}e\n# shunt-env:OK=yes\necho hi\n").unwrap();
        let td = super::task_directives(&task);
        assert_eq!(td.workdir, None, "NUL workdir directive must be dropped");
        assert_eq!(td.envs, vec![("OK".to_string(), "yes".to_string())]);
        std::fs::remove_dir_all(&root).ok();
    }

    /// An indented `#` is still a comment to the shell, so it must
    /// neither end the §3.13 head scan nor hide a directive — the
    /// same trim_start rule read_env_file already applies. A plain
    /// indented comment between directives keeps the scan going, and
    /// an indented `# shunt-env:` is honored outright.
    #[test]
    fn indented_comments_do_not_end_directive_head() {
        let task = std::env::temp_dir().join(format!("shunt-indent-{}", std::process::id()));
        std::fs::write(&task, "#!/usr/bin/env bash\n# shunt-env:EARLY=1\n  # an indented note\n\t# shunt-env:LATE=2\n   # shunt:timeout_s=60\necho hi\n# shunt-env:NEVER=seen\n").unwrap();
        let td = super::task_directives(&task);
        assert_eq!(td.envs, vec![("EARLY".to_string(), "1".to_string()), ("LATE".to_string(), "2".to_string())]);
        assert_eq!(td.timeout_s, Some(60));
        std::fs::remove_file(&task).ok();
    }

    /// `TASK_THREADS` is literal env data (§3.12): padded `auto `
    /// still resolves to the cgroup probe, but a value with no
    /// digits caps at 1 *and* says so in the task log instead of
    /// silently degrading the task.
    #[test]
    fn task_threads_garbage_caps_loudly_and_padded_auto_works() {
        for (value, expect_warn) in [("x", true), ("auto ", false)] {
            let root = std::env::temp_dir().join(format!("shunt-threads-{}-{}", std::process::id(), value.replace(' ', "_")));
            std::fs::create_dir_all(root.join("running")).unwrap();
            std::fs::write(root.join("running").join("t.sh"), "#!/usr/bin/env bash\necho \"OMP=$OMP_NUM_THREADS\"\n").unwrap();
            std::fs::write(root.join("env"), format!("TASK_THREADS={value}\n")).unwrap();
            let (mut child, _) = super::spawn_task(&root, "t").unwrap();
            assert_eq!(child.wait().unwrap().code(), Some(0));
            let log = std::fs::read_to_string(root.join("running").join("t.sh.log")).unwrap();
            let omp = log.lines().find_map(|l| l.strip_prefix("OMP=")).and_then(|v| v.parse::<u32>().ok());
            if expect_warn {
                assert!(log.contains("=== warn: TASK_THREADS=\"x\" has no digits; capping threads to 1"), "log: {log}");
                assert_eq!(omp, Some(1), "log: {log}");
            } else {
                assert!(!log.contains("has no digits"), "log: {log}");
                assert!(omp.unwrap_or(0) >= 1, "log: {log}");
            }
            std::fs::remove_dir_all(&root).ok();
        }
    }

    /// A NUL-bearing `<qroot>/shell` must fall back to bash: spawn
    /// must return a live child, never panic inside dispatch.
    #[test]
    fn nul_shell_file_falls_back_to_bash() {
        let root = std::env::temp_dir().join(format!("shunt-nulshell-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(root.join("running")).unwrap();
        std::fs::write(root.join("running").join("t.sh"), "#!/usr/bin/env bash\nexit 0\n").unwrap();
        std::fs::write(root.join("shell"), "bash\u{0}evil").unwrap();
        let (mut child, _) = super::spawn_task(&root, "t").expect("spawn_task must not fail on NUL shell file");
        assert_eq!(child.wait().unwrap().code(), Some(0));
        std::fs::remove_dir_all(&root).ok();
    }

    /// Quarantine with a missing source is a no-op: when the entry
    /// already left the track (e.g. finalize's rename succeeded and
    /// only the exit-file write failed) no bogus `failed/<name>/`
    /// holding only `shunt_error` may appear beside the real entry
    /// in done/.
    #[test]
    fn quarantine_missing_source_is_noop() {
        let root = std::env::temp_dir().join(format!("shunt-qnoop-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(root.join("running")).unwrap();
        std::fs::create_dir_all(root.join("done")).unwrap();
        // Real entry already finalized into done/.
        std::fs::create_dir_all(root.join("done").join("0001_t")).unwrap();
        super::quarantine(&root, "running", "0001_t", "exit write failed").unwrap();
        assert!(!root.join("failed").join("0001_t").exists(), "ghost failed/ entry was created");
        assert!(root.join("done").join("0001_t").exists());
        std::fs::remove_dir_all(&root).ok();
    }

    /// `exit`/`timeout` are written inside the running entry BEFORE
    /// the rename (design.md §3.9 deviates: write-then-mv): on
    /// success the sidecars simply travel with the dir to done/.
    #[test]
    fn finalize_writes_exit_inside_dir_entry() {
        let root = std::env::temp_dir().join(format!("shunt-fin-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        let entry = root.join("running").join("0001_t");
        std::fs::create_dir_all(&entry).unwrap();
        std::fs::write(entry.join("task.sh"), "exit 0\n").unwrap();
        std::fs::write(entry.join("log"), "=== task 0001_t\n").unwrap();
        std::fs::write(entry.join("pid"), "12345\n").unwrap();
        super::finalize(&root, "0001_t", 0, None).unwrap();
        let done = root.join("done").join("0001_t");
        assert_eq!(std::fs::read_to_string(done.join("exit")).unwrap(), "0\n");
        assert!(!done.join("pid").exists());
        assert!(!entry.exists());
        std::fs::remove_dir_all(&root).ok();
    }

    /// A failed `exit` write must leave the entry still in running/
    /// so the caller's quarantine lands it in failed/ with a
    /// `shunt_error` — never an exit-less terminal entry that
    /// `recent_exits` filters into `unknown`.
    #[test]
    fn finalize_exit_write_failure_leaves_entry_in_running() {
        let root = std::env::temp_dir().join(format!("shunt-finfail-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        let entry = root.join("running").join("0001_t");
        std::fs::create_dir_all(&entry).unwrap();
        std::fs::write(entry.join("task.sh"), "exit 0\n").unwrap();
        // `exit` as a directory makes fs::write fail.
        std::fs::create_dir(entry.join("exit")).unwrap();
        assert!(super::finalize(&root, "0001_t", 0, None).is_err());
        assert!(entry.exists(), "entry left running/ despite failed exit write");
        assert!(!root.join("done").join("0001_t").exists());
        // The caller's fallback still sees a real entry to quarantine.
        super::quarantine(&root, "running", "0001_t", "finalize failed (rc 0)").unwrap();
        assert!(root.join("failed").join("0001_t").join("shunt_error").exists());
        std::fs::remove_dir_all(&root).ok();
    }

    /// A failed task.sh rename on a legacy flat entry must not strand
    /// the just-created `dest` holding only `exit`: `list_entries`
    /// counts any dir and `recent_exits` reads its `exit`, so the
    /// task would report done (rc 0) while the caller's quarantine
    /// also lands the flat remainder in failed/ — a phantom
    /// double-outcome.
    #[test]
    fn finalize_flat_tasksh_rename_failure_removes_dest() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("shunt-finflat-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        let running = root.join("running");
        std::fs::create_dir_all(&running).unwrap();
        std::fs::write(running.join("0001_t.sh"), "exit 0\n").unwrap();
        std::fs::write(running.join("0001_t.sh.log"), "=== task 0001_t\n").unwrap();
        std::fs::write(running.join("0001_t.sh.pid"), "12345\n").unwrap();
        // Rename needs write perm on both parent dirs; read-only
        // running/ fails the rename AFTER dest/exit were written.
        std::fs::set_permissions(&running, std::fs::Permissions::from_mode(0o555)).unwrap();
        assert!(super::finalize(&root, "0001_t", 0, None).is_err());
        std::fs::set_permissions(&running, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!root.join("done").join("0001_t").exists(), "partial done/ entry survived failed rename");
        // The flat source is untouched so quarantine can still move it.
        assert!(running.join("0001_t.sh").exists());
        super::quarantine(&root, "running", "0001_t", "finalize failed (rc 0)").unwrap();
        assert!(root.join("failed").join("0001_t").join("task.sh").exists());
        std::fs::remove_dir_all(&root).ok();
    }

    /// Reschedule fixture: a running/ dir entry with a retry
    /// directive task.sh plus log + pid sidecars.
    fn running_entry(root: &std::path::Path, base: &str, head: &str, log: &str) {
        let entry = root.join("running").join(base);
        std::fs::create_dir_all(&entry).unwrap();
        std::fs::write(entry.join("task.sh"), head).unwrap();
        std::fs::write(entry.join("log"), log).unwrap();
        std::fs::write(entry.join("pid"), "1\n").unwrap();
    }

    /// rc 75 on a retry-declared task requeues into pending/ under
    /// the SAME name — the ledger's queue_name keeps resolving —
    /// with attempts/retry_after sidecars and the failed log swept
    /// out (spawn truncates `log` on the rerun).
    #[test]
    fn reschedule_rc75_requeues_same_name() {
        let root = std::env::temp_dir().join(format!("shunt-rs75-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(root.join("pending")).unwrap();
        std::fs::create_dir_all(root.join("failed")).unwrap();
        let head = "#!/usr/bin/env bash\n# shunt:retry=2\n# shunt:retry_delay_s=7\nexec false\n";
        running_entry(&root, "0100_t", head, "=== task 0100_t\nbusy\n");
        let td = super::task_directives(&root.join("running/0100_t/task.sh"));
        match super::maybe_reschedule(&root, "0100_t", 75, &td).unwrap() {
            super::Resched::Queued { attempt, via, .. } => {
                assert_eq!(attempt, 1);
                assert_eq!(via, "rc75");
            }
            _ => panic!("rc 75 + retry=2 must requeue"),
        }
        let entry = root.join("pending/0100_t");
        assert!(entry.is_dir(), "entry did not land back in pending/");
        assert_eq!(std::fs::read_to_string(entry.join("attempts")).unwrap(), "1\n");
        let ra: u64 = std::fs::read_to_string(entry.join("retry_after")).unwrap().trim().parse().unwrap();
        assert!(ra >= crate::proto::time::unix_now() + 6, "retry_after not ~7s out: {ra}");
        assert!(!entry.join("pid").exists());
        assert!(root.join("failed/0100_t.attempt1.log").exists(), "failed log not swept");
        assert!(!entry.join("log").exists());
        std::fs::remove_dir_all(&root).ok();
    }

    /// The OOM heuristic is bounded by declaration: a retry task
    /// dying rc 1 with an OOM traceback requeues via the heuristic,
    /// while a task WITHOUT `# shunt:retry=` saying the same words
    /// still finalizes — the estimate never turns into a silent
    /// retry of an arbitrary failure.
    #[test]
    fn reschedule_oom_heuristic_only_for_retry_tasks() {
        let root = std::env::temp_dir().join(format!("shunt-rsom-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(root.join("pending")).unwrap();
        let oom_log = "=== task 0100_t\ntorch.OutOfMemoryError: CUDA out of memory. Tried to allocate 1.50 GiB\n";
        running_entry(&root, "0100_t", "#!/usr/bin/env bash\n# shunt:retry=1\nexec false\n", oom_log);
        let td = super::task_directives(&root.join("running/0100_t/task.sh"));
        match super::maybe_reschedule(&root, "0100_t", 1, &td).unwrap() {
            super::Resched::Queued { via, .. } => assert_eq!(via, "oom_heuristic"),
            _ => panic!("OOM traceback + retry=1 must requeue"),
        }
        // Same log, no retry declared -> No.
        running_entry(&root, "0200_u", "#!/usr/bin/env bash\nexec false\n", oom_log);
        let td = super::task_directives(&root.join("running/0200_u/task.sh"));
        assert!(matches!(super::maybe_reschedule(&root, "0200_u", 1, &td).unwrap(), super::Resched::No));
        // rc 1, retry declared, but log is clean -> No.
        running_entry(&root, "0300_v", "#!/usr/bin/env bash\n# shunt:retry=1\nexec false\n", "=== task\nsyntax error near token\n");
        let td = super::task_directives(&root.join("running/0300_v/task.sh"));
        assert!(matches!(super::maybe_reschedule(&root, "0300_v", 1, &td).unwrap(), super::Resched::No));
        std::fs::remove_dir_all(&root).ok();
    }

    /// Lock acquisition and sidecar failures must leave the entry
    /// finalizable, and an acquired lock must not escape the error path.
    #[test]
    fn reschedule_errors_release_lock_and_allow_finalize() {
        use fs2::FileExt;

        for fault in ["lock", "sidecar"] {
            let root = std::env::temp_dir().join(format!("shunt-rsfail-{fault}-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
            crate::agent::queue::init(&root).unwrap();
            let head = "#!/usr/bin/env bash\n# shunt:id=reschedule-error\n# shunt:retry=1\nexec false\n";
            running_entry(&root, "0100_t", head, "busy\n");
            let entry = root.join("running/0100_t");
            if fault == "lock" {
                std::fs::create_dir(root.join(".seq.lock")).unwrap();
            } else {
                std::fs::create_dir(entry.join("attempts")).unwrap();
            }
            let td = super::task_directives(&entry.join("task.sh"));
            assert!(super::maybe_reschedule(&root, "0100_t", 75, &td).is_err());
            assert!(entry.join("task.sh").exists());
            assert!(entry.join("pid").exists());
            assert_eq!(std::fs::read_to_string(entry.join("log")).unwrap(), "busy\n");
            assert!(!entry.join("retry_after").exists());
            assert!(crate::agent::queue::pending_list(&root).unwrap().is_empty());
            if fault == "lock" {
                std::fs::remove_dir(root.join(".seq.lock")).unwrap();
                assert!(!entry.join("attempts").exists());
            }
            let lock = std::fs::OpenOptions::new().create(true).append(true).open(root.join(".seq.lock")).unwrap();
            lock.try_lock_exclusive().expect("reschedule leaked .seq.lock on error");
            drop(lock);
            // The daemon takes this fallback after maybe_reschedule
            // returns Err; it must preserve the failure and delivery id.
            super::finalize(&root, "0100_t", 75, None).unwrap();
            assert_eq!(std::fs::read_to_string(root.join("failed/0100_t/exit")).unwrap(), "75\n");
            assert!(!entry.exists());
            let replay = crate::agent::queue::enqueue(&root, "copy", head).unwrap();
            assert!(replay.deduped());
            assert_eq!(replay.name(), "0100_t");
            std::fs::remove_dir_all(&root).unwrap();
        }
    }

    /// `attempts` reaching the declared budget finalizes instead of
    /// requeueing — retry=N means N extra runs, not N failures.
    #[test]
    fn reschedule_budget_exhaustion_finalizes() {
        let root = std::env::temp_dir().join(format!("shunt-rsex-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(root.join("pending")).unwrap();
        running_entry(&root, "0100_t", "#!/usr/bin/env bash\n# shunt:retry=1\nexec false\n", "=== task\n");
        std::fs::write(root.join("running/0100_t/attempts"), "1\n").unwrap();
        let td = super::task_directives(&root.join("running/0100_t/task.sh"));
        assert!(matches!(super::maybe_reschedule(&root, "0100_t", 75, &td).unwrap(), super::Resched::No));
        assert!(root.join("running/0100_t").exists(), "exhausted entry must stay for finalize");
        std::fs::remove_dir_all(&root).ok();
    }
}
