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
pub fn read_env_file(root: &Path) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    if let Ok(text) = std::fs::read_to_string(root.join("env")) {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                map.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
    }
    map
}

/// Spawn a claimed task under its own process group (pgid = leader
/// pid), output appended to `running/<name>.log`. The `.pid` file is
/// written by the daemon right after this returns.
pub fn spawn_task(root: &Path, task_name: &str) -> Result<Child> {
    let running = root.join("running");
    let task = running.join(task_name);
    let log_path = running.join(format!("{task_name}.log"));

    {
        let mut log = std::fs::OpenOptions::new().create(true).write(true).truncate(true).open(&log_path)?;
        writeln!(log, "=== task {task_name}")?;
        writeln!(log, "=== start {}", queue::iso8601_now())?;
    }

    let env = read_env_file(root);
    let (task_workdir, task_envs) = task_directives(&task);
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
    cmd.spawn().context("spawn task failed")
}

/// Parse the task file's comment head for per-task overrides
/// (design.md §3.13):
///   `# shunt:workdir=<abs path>`  — runner cwd for this task only
///   `# shunt-env:<KEY>=<value>`   — env overlay, beats queue `env`
/// Directives live in the comment head only; scanning stops at the
/// first non-comment, non-blank line.
fn task_directives(task: &Path) -> (Option<String>, Vec<(String, String)>) {
    let Ok(text) = std::fs::read_to_string(task) else {
        return (None, Vec::new());
    };
    let mut workdir = None;
    let mut envs = Vec::new();
    for line in text.lines() {
        if let Some(w) = line.strip_prefix("# shunt:workdir=") {
            workdir = Some(w.trim().to_string());
        } else if let Some(kv) = line.strip_prefix("# shunt-env:") {
            if let Some((k, v)) = kv.split_once('=') {
                envs.push((k.trim().to_string(), v.to_string()));
            }
        } else if !line.starts_with('#') && !line.trim().is_empty() {
            break;
        }
    }
    (workdir, envs)
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

/// TERM → 10×1 s → KILL on the task's recorded pgid. The pgid comes
/// only from `running/<task>.pid`, never from a name match
/// (design.md §3.1). Returns `(pgid, escalated)`.
pub fn kill_task(root: &Path, task_name: &str) -> Result<(u32, bool)> {
    let pidf = root.join("running").join(format!("{task_name}.pid"));
    let pgid: u32 = std::fs::read_to_string(&pidf).with_context(|| format!("no pid file for {task_name}"))?.trim().parse().context("bad pid file")?;
    signal_pgid(pgid, "TERM");
    for _ in 0..10 {
        if !pgid_alive(pgid) {
            return Ok((pgid, false));
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    signal_pgid(pgid, "KILL");
    Ok((pgid, true))
}

/// Finalize a reaped task: `=== exit` trailer, `.exit` file, mv task +
/// log to done/ (rc 0) or failed/.
pub fn finalize(root: &Path, base: &str, rc: i32) -> Result<()> {
    let running = root.join("running");
    let log = running.join(format!("{base}.log"));
    let dest = root.join(if rc == 0 { "done" } else { "failed" });
    std::fs::create_dir_all(&dest)?;
    if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&log) {
        let _ = writeln!(f, "=== exit {rc} {}", queue::iso8601_now());
    }
    let _ = std::fs::remove_file(running.join(format!("{base}.pid")));
    std::fs::rename(running.join(base), dest.join(base)).with_context(|| format!("cannot move {base} to {}", dest.display()))?;
    if log.exists() {
        std::fs::rename(&log, dest.join(format!("{base}.log")))?;
    }
    std::fs::write(dest.join(format!("{base}.exit")), format!("{rc}\n"))?;
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

/// Render a task body from metadata (single generator, shared by
/// `new`/`mktask`/submit forwarding; design.md §3.13–3.14).
/// No `cd` line: the runner places cwd at the binding's workdir.
pub fn render_task(meta: &crate::proto::task::TaskMeta) -> String {
    let mut out = String::from("#!/usr/bin/env bash\n");
    out.push_str(&format!("# task: {} (generated by shunt {})\n", meta.slug, queue::iso8601_now()));
    // Per-task directives ride in the comment head; exec.rs applies
    // them at spawn (the wire carries only the body).
    if let Some(w) = &meta.workdir {
        out.push_str(&format!("# shunt:workdir={w}\n"));
    }
    for (k, v) in &meta.env {
        out.push_str(&format!("# shunt-env:{k}={v}\n"));
    }
    out.push_str("set -uo pipefail\n");
    if let Some(guard) = &meta.guard {
        let q = crate::server::transport::shell_quote(guard);
        out.push_str(&format!("if [ -e {q} ]; then echo \"guard hit: {guard} exists; skipping\"; exit 0; fi\n"));
    }
    let cmd = meta.cmd.iter().map(|a| crate::server::transport::shell_quote(a)).collect::<Vec<_>>().join(" ");
    out.push_str(&format!("exec {cmd}\n"));
    out
}

/// Convenience: the path of a running task's log.
pub fn running_log(root: &Path, base: &str) -> PathBuf {
    root.join("running").join(format!("{base}.log"))
}
