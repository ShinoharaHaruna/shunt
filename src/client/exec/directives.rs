//! Per-task `# shunt:`/`# shunt-env:` directives parsed from the
//! comment head (design.md §3.13).

use std::path::Path;

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
