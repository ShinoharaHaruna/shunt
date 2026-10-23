//! Task spawn: process-group leader, env layering, log header
//! (design.md §3.7, §3.12).

use anyhow::{Context, Result};
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};

use super::directives::{task_directives, TaskDirectives};
use super::env::read_env_file;
use crate::client::queue;

/// Proxy vars every task loses; a task that needs a tunnel re-exports
/// it inside itself (design.md §3.12).
const PROXY_VARS: [&str; 6] = ["ALL_PROXY", "all_proxy", "http_proxy", "https_proxy", "HTTP_PROXY", "HTTPS_PROXY"];

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
            crate::client::probe::cgroup_cores().unwrap_or(1)
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
