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
                // `Command::env` panics on an empty key — a `=foo`
                // line must be dropped here, not crash the daemon
                // mid-dispatch.
                if !k.is_empty() {
                    map.insert(k.to_string(), v.trim().to_string());
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
    let shell = std::fs::read_to_string(root.join("shell")).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).unwrap_or_else(|| "bash".into());
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
    if let Some(prepend) = env.get("PATH_PREPEND") {
        let cur = std::env::var("PATH").unwrap_or_default();
        cmd.env("PATH", format!("{prepend}:{cur}"));
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
            workdir = Some(w.trim().to_string());
        } else if let Some(t) = line.strip_prefix("# shunt:timeout_s=") {
            timeout_s = t.trim().parse::<u64>().ok().filter(|&n| n > 0);
        } else if let Some(kv) = line.strip_prefix("# shunt-env:") {
            if let Some((k, v)) = kv.split_once('=') {
                let k = k.trim();
                // Same panic guard as the env file: `# shunt-env:=x`
                // yields an empty key that `Command::env` rejects.
                if !k.is_empty() {
                    // Trim like the env file: same literal semantics
                    // for the two KEY=value surfaces.
                    envs.push((k.to_string(), v.trim().to_string()));
                }
            }
        } else if !line.starts_with('#') && !line.trim().is_empty() {
            break;
        }
    }
    (workdir, envs, timeout_s)
}

/// Send a signal to a whole process group (`kill -SIG -- -pgid`).
/// Returns false when the group is gone.
pub fn signal_pgid(pgid: u32, sig: &str) -> bool {
    Command::new("kill").arg(format!("-{sig}")).arg("--").arg(format!("-{pgid}")).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().map(|s| s.success()).unwrap_or(false)
}

/// Is any member of process group `pgid` still alive?
pub fn pgid_alive(pgid: u32) -> bool {
    signal_pgid(pgid, "0")
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
    signal_pgid(pgid, "TERM");
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
    signal_pgid(pgid, "KILL");
    Ok(KillOutcome::Signaled { pgid, escalated: true })
}

/// Quarantine: move a stuck entry to `failed/` with a `shunt_error`
/// note — the catch-all exit for entries that can neither run nor be
/// left invisible (claim collisions, failed finalize/requeue
/// renames). A same-name failed/ entry gets a timestamped suffix
/// rather than being clobbered.
pub fn quarantine(root: &Path, track: &str, name: &str, note: &str) -> Result<()> {
    let failed = root.join("failed");
    std::fs::create_dir_all(&failed)?;
    let mut dest = failed.join(name);
    if dest.exists() || failed.join(format!("{name}.sh")).exists() {
        dest = failed.join(format!("{name}.quarantined.{}", queue::unix_now()));
    }
    let src_dir = root.join(track).join(name);
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
        let _ = std::fs::remove_file(running.join(base).join("pid"));
        std::fs::rename(running.join(base), &dest).with_context(|| format!("cannot move {base} to {}", dest.display()))?;
    } else {
        // Legacy flat entry: mkdir the entry dir and drop the pieces in.
        std::fs::create_dir_all(&dest)?;
        std::fs::rename(running.join(format!("{base}.sh")), dest.join("task.sh")).with_context(|| format!("cannot move {base} to {}", dest.display()))?;
        let log = running.join(format!("{base}.sh.log"));
        if log.exists() {
            std::fs::rename(&log, dest.join("log"))?;
        }
        let _ = std::fs::remove_file(running.join(format!("{base}.sh.pid")));
    }
    std::fs::write(dest.join("exit"), format!("{rc}\n"))?;
    if let Some(limit) = timeout_s {
        let _ = std::fs::write(dest.join("timeout"), format!("{limit}\n"));
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
        out.push_str(&format!("if [ -e {q} ]; then echo \"guard hit: {q} exists; skipping\"; exit 0; fi\n"));
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

    /// A hostile guard path must reach the echo line only in its
    /// shell-quoted form: raw `$()`, backticks, or `"` inside the
    /// double-quoted message would execute or break out when the
    /// guard-hit branch runs.
    #[test]
    fn guard_echo_uses_quoted_form() {
        for guard in ["runs/$(touch X).json", "runs/evil\".json", "runs/`id`.json"] {
            let meta = crate::proto::task::TaskMeta { slug: "t".into(), guard: Some(guard.into()), workdir: None, env: Default::default(), cmd: vec!["true".into()], timeout_s: None };
            let body = super::render_task(&meta);
            let q = crate::server::transport::shell_quote(guard);
            let line = format!("if [ -e {q} ]; then echo \"guard hit: {q} exists; skipping\"; exit 0; fi");
            assert!(body.contains(&line), "body missing quoted guard line for {guard:?}: {body}");
            assert!(!body.contains(&format!("echo \"guard hit: {guard}")), "raw guard leaked into echo for {guard:?}");
        }
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
}
