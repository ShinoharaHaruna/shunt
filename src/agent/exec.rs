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
/// preserved verbatim. The first `=` splits key/value; surrounding
/// whitespace is trimmed.
pub fn read_env_file(root: &Path) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    if let Ok(text) = std::fs::read_to_string(root.join("env")) {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                let k = k.trim();
                let v = v.trim();
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
/// `Option<u64>` is the task's wall-clock budget from its
/// `# shunt:timeout_s=` head directive — the daemon counts it from
/// spawn and a daemon restart re-times the requeued rerun wholesale
/// (at-least-once, design.md §3.6).
pub fn spawn_task(root: &Path, task_name: &str) -> Result<(Child, Option<u64>)> {
    let task = queue::task_file(root, "running", task_name);
    let log_path = queue::piece_file(root, "running", task_name, "log");

    {
        let mut log = std::fs::OpenOptions::new().create(true).write(true).truncate(true).open(&log_path)?;
        writeln!(log, "=== task {task_name}")?;
        writeln!(log, "=== start {}", queue::iso8601_now())?;
    }

    let env = read_env_file(root);
    let (task_workdir, task_envs, timeout_s) = task_directives(&task);
    let workdir = task_workdir.or_else(|| env.get("WORKDIR").cloned()).unwrap_or_else(|| root.to_string_lossy().into_owned());

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
        let n = if threads == "auto" { crate::agent::probe::cgroup_cores().unwrap_or(1) } else { queue::digits_only(threads).unwrap_or(1) };
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
    for (k, v) in &task_envs {
        cmd.env(k, v);
    }
    cmd.spawn().map(|c| (c, timeout_s)).context("spawn task failed")
}

/// Parse the task file's comment head for per-task overrides
/// (design.md §3.13):
///   `# shunt:workdir=<abs path>`  — runner cwd for this task only
///   `# shunt-env:<KEY>=<value>`   — env overlay, beats queue `env`
///   `# shunt:timeout_s=<secs>`    — wall-clock budget; 0 = unlimited
/// Directives live in the comment head only; scanning stops at the
/// first non-comment, non-blank line. Unknown `# shunt:` lines are
/// skipped silently, so a task pushed to an older runner degrades to
/// a no-op comment (backward compat).
fn task_directives(task: &Path) -> (Option<String>, Vec<(String, String)>, Option<u64>) {
    let Ok(text) = std::fs::read_to_string(task) else {
        return (None, Vec::new(), None);
    };
    let mut workdir = None;
    let mut envs = Vec::new();
    let mut timeout_s = None;
    for line in text.lines() {
        if let Some(w) = line.strip_prefix("# shunt:workdir=") {
            // NUL would panic `Command::arg` at spawn — drop the
            // directive so the env-file/default workdir applies.
            let w = w.trim();
            if !w.contains('\0') {
                workdir = Some(w.to_string());
            }
        } else if let Some(t) = line.strip_prefix("# shunt:timeout_s=") {
            timeout_s = t.trim().parse::<u64>().ok().filter(|&n| n > 0);
        } else if let Some(kv) = line.strip_prefix("# shunt-env:") {
            if let Some((k, v)) = kv.split_once('=') {
                let k = k.trim();
                let v = v.trim();
                // Same panic guard as the env file: `# shunt-env:=x`
                // yields an empty key and a NUL in either half is a
                // `Command::env` panic — both must be dropped.
                if !k.is_empty() && !k.contains('\0') && !v.contains('\0') {
                    // Trim like the env file: same literal semantics
                    // for the two KEY=value surfaces.
                    envs.push((k.to_string(), v.to_string()));
                }
            }
        } else if !line.starts_with('#') && !line.trim().is_empty() {
            break;
        }
    }
    (workdir, envs, timeout_s)
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
    let pgid = it.next()?.parse().ok()?;
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

/// TERM → 10×1 s → KILL on the task's recorded pgid. The pgid comes
/// only from the entry's `pid` file, never from a name match, and is
/// verified as the task's leader before every signal — a reaped pid
/// recycled into a foreign process group is reported `Gone`, not
/// signaled (design.md §3.1).
pub fn kill_task(root: &Path, task_name: &str) -> Result<KillOutcome> {
    let (pgid, start) = read_pid_file(root, "running", task_name).with_context(|| format!("no pid file for {task_name}"))?;
    if !pgid_is_task_leader(pgid, start) {
        return Ok(KillOutcome::Gone);
    }
    // A TERM that cannot be sent (EPERM, bad arg) is an error, not
    // a silent Signaled — the kill command must not claim a signal
    // it never delivered (design.md §3.9a).
    match signal_pgid(pgid, "TERM") {
        SignalOutcome::Delivered | SignalOutcome::Gone => {}
        SignalOutcome::Failed(e) => return Err(e).with_context(|| format!("cannot TERM pgid {pgid}")),
    }
    for _ in 0..10 {
        if !pgid_alive(pgid) {
            return Ok(KillOutcome::Signaled { pgid, escalated: false });
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    // Re-verify before escalating: the group may have died on TERM
    // and the pgid been recycled during the grace window.
    if !pgid_is_task_leader(pgid, start) {
        return Ok(KillOutcome::Signaled { pgid, escalated: false });
    }
    match signal_pgid(pgid, "KILL") {
        SignalOutcome::Delivered | SignalOutcome::Gone => Ok(KillOutcome::Signaled { pgid, escalated: true }),
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
        dest = failed.join(format!("{name}.quarantined.{}", queue::unix_now()));
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
        let _ = writeln!(f, "=== exit {rc} {}", queue::iso8601_now());
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
        std::fs::create_dir_all(&dest)?;
        std::fs::write(dest.join("exit"), format!("{rc}\n"))?;
        if let Some(limit) = timeout_s {
            let _ = std::fs::write(dest.join("timeout"), format!("{limit}\n"));
        }
        std::fs::rename(running.join(format!("{base}.sh")), dest.join("task.sh")).with_context(|| format!("cannot move {base} to {}", dest.display()))?;
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
    out.push_str(&format!("# task: {} (generated by shunt {})\n", meta.slug, queue::iso8601_now()));
    // Per-task directives ride in the comment head; exec.rs applies
    // them at spawn (the wire carries only the body).
    if let Some(w) = &meta.workdir {
        out.push_str(&format!("# shunt:workdir={w}\n"));
    }
    if let Some(t) = meta.timeout_s.filter(|&n| n > 0) {
        out.push_str(&format!("# shunt:timeout_s={t}\n"));
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

    /// A hostile guard path must reach the guard line only as a
    /// single-quoted printf argv: inside a double-quoted message the
    /// quotes are literal characters and `$()`, backticks, or `"`
    /// would still expand when the guard-hit branch runs.
    #[test]
    fn guard_echo_uses_quoted_form() {
        for guard in ["runs/$(touch X).json", "runs/evil\".json", "runs/`id`.json"] {
            let meta = crate::proto::task::TaskMeta { slug: "t".into(), guard: Some(guard.into()), workdir: None, env: Default::default(), cmd: vec!["true".into()], timeout_s: None };
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
        let meta = crate::proto::task::TaskMeta { slug: "t".into(), guard: Some("x$(touch PWNED).json".into()), workdir: None, env: Default::default(), cmd: vec!["true".into()], timeout_s: None };
        let body = super::render_task(&meta);
        let out = std::process::Command::new("bash").arg("-c").arg(&body).current_dir(&root).output().unwrap();
        assert!(out.status.success(), "bash exited {:?}: {}", out.status, String::from_utf8_lossy(&out.stderr));
        assert!(!pwned.exists(), "guard payload executed: {body}");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("guard hit: x$(touch PWNED).json exists; skipping"), "unexpected stdout: {stdout}");
        std::fs::remove_dir_all(&root).ok();
    }

    /// The env file is literal KEY=value (design.md §3.12): spaces
    /// survive verbatim but quotes are never stripped — `K="a b"`
    /// keeps the quote characters as part of the value.
    #[test]
    fn env_values_are_literal_no_quote_semantics() {
        let root = std::env::temp_dir().join(format!("shunt-envtest-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("env"), "# comment\n\nFOO=a b c\nQUOTED=\"a b\"\nSQUOTED='x'\nSPACED =  padded  \nNOEQ\n").unwrap();
        let m = read_env_file(&root);
        assert_eq!(m["FOO"], "a b c");
        assert_eq!(m["QUOTED"], "\"a b\"");
        assert_eq!(m["SQUOTED"], "'x'");
        assert_eq!(m["SPACED"], "padded");
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
        std::fs::write(&task, "#!/usr/bin/env bash\n# shunt-env:=bad\n# shunt-env:OK=yes\necho hi\n").unwrap();
        let (_, envs, _) = super::task_directives(&task);
        assert_eq!(envs, vec![("OK".to_string(), "yes".to_string())]);
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
        let (workdir, envs, _) = super::task_directives(&task);
        assert_eq!(workdir, None, "NUL workdir directive must be dropped");
        assert_eq!(envs, vec![("OK".to_string(), "yes".to_string())]);
        std::fs::remove_dir_all(&root).ok();
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
}
