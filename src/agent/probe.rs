//! Machine facts probe (design.md §8). cgroup-aware: `cpu.max` /
//! `cpu.cfs_quota_us` for cores, `memory.max` / `memory.limit_in_bytes`
//! for memory; `nproc`/`free` only as fallback and marked suspect.

use crate::proto::wire::ProbeReport;
use std::process::Command;

fn run(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    if out.status.success() {
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        None
    }
}

fn read(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

/// Cgroup-aware CPU count: `cpu.max` (v2) or cfs quota/period (v1);
/// `nproc` only as a suspect fallback (it reports the host on AutoDL).
pub fn cgroup_cores() -> Option<u32> {
    if let Some(max) = read("/sys/fs/cgroup/cpu.max") {
        let mut it = max.split_whitespace();
        let quota = it.next()?;
        let period: u64 = it.next().and_then(|p| p.parse().ok()).unwrap_or(100_000);
        if quota != "max" {
            if let Ok(q) = quota.parse::<u64>() {
                if period > 0 {
                    return Some((q.div_ceil(period)).max(1) as u32);
                }
            }
        }
    }
    let quota = read("/sys/fs/cgroup/cpu/cpu.cfs_quota_us").and_then(|s| s.parse::<i64>().ok());
    let period = read("/sys/fs/cgroup/cpu/cpu.cfs_period_us").and_then(|s| s.parse::<u64>().ok());
    if let (Some(q), Some(p)) = (quota, period) {
        if q > 0 && p > 0 {
            return Some(((q as u64).div_ceil(p)).max(1) as u32);
        }
    }
    run("nproc", &[]).and_then(|s| s.trim().parse().ok())
}

/// Cgroup-aware memory in GiB; falls back to /proc/meminfo (host value
/// on AutoDL — suspect, like `nproc`).
fn cgroup_mem_gb() -> Option<f64> {
    const GIB: f64 = 1_073_741_824.0;
    if let Some(max) = read("/sys/fs/cgroup/memory.max") {
        if let Ok(b) = max.parse::<u64>() {
            return Some(b as f64 / GIB);
        }
    }
    if let Some(b) = read("/sys/fs/cgroup/memory/memory.limit_in_bytes").and_then(|s| s.parse::<u64>().ok()) {
        // v1 reports ~i64::MAX when unbounded
        if b < (1u64 << 60) {
            return Some(b as f64 / GIB);
        }
    }
    read("/proc/meminfo").and_then(|s| s.lines().find(|l| l.starts_with("MemTotal:")).and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok()).map(|kb| kb as f64 * 1024.0 / GIB))
}

fn nvidia_smi() -> Option<(Option<String>, Option<String>, Option<String>)> {
    let q = run("nvidia-smi", &["--query-gpu=name,driver_version", "--format=csv,noheader"]);
    let (gpu, driver) = q.as_deref().and_then(|s| s.lines().next()).and_then(|l| l.split_once(',')).map(|(g, d)| (g.trim().to_string(), d.trim().to_string())).map(|(g, d)| (Some(g), Some(d))).unwrap_or((None, None));
    // CUDA version only appears in the header output
    let cuda = run("nvidia-smi", &[]).and_then(|s| {
        let marker = "CUDA Version:";
        let pos = s.find(marker)?;
        s[pos + marker.len()..].split_whitespace().next().map(|v| v.trim_end_matches('|').to_string())
    });
    if gpu.is_none() && cuda.is_none() {
        return None;
    }
    Some((gpu, driver, cuda))
}

/// Parse `df -Pk` output: take the Available column (4th field) from
/// the last line and convert 1 KiB blocks to GiB. `-P` forbids line
/// wrapping, so the last line is always the data row; still fail
/// gracefully on short/garbage lines.
fn parse_df_pk(out: &str) -> Option<f64> {
    let line = out.lines().last()?;
    let kib: u64 = line.split_whitespace().nth(3)?.parse().ok()?;
    Some(kib as f64 / 1_048_576.0)
}

fn disk_free_gb(path: &str) -> Option<f64> {
    // POSIX `df -Pk`: busybox-compatible, unlike GNU-only -B1/--output.
    let out = run("df", &["-Pk", path])?;
    parse_df_pk(&out)
}

/// Can this box do root things: real root (`id -u` = 0) or
/// passwordless sudo. Feeds the `can_root` fact (design.md §8).
fn can_root() -> Option<bool> {
    let uid = run("id", &["-u"])?;
    if uid.trim() == "0" {
        return Some(true);
    }
    let ok = Command::new("sudo").args(["-n", "true"]).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().map(|s| s.success()).unwrap_or(false);
    Some(ok)
}

/// Best-guess poweroff command for `facts.shutdown_cmd` (design.md
/// §8): `systemctl poweroff` when systemd is live, else `shutdown`,
/// else `poweroff`, else none. Advisory — a container may have the
/// binary but lack CAP_SYS_BOOT; the row's `shutdown_cmd` overrides.
fn shutdown_cmd() -> Option<String> {
    let have = |name: &str| Command::new("sh").args(["-c", &format!("command -v {name} >/dev/null 2>&1")]).status().map(|s| s.success()).unwrap_or(false);
    if std::path::Path::new("/run/systemd/system").is_dir() && have("systemctl") {
        Some("systemctl poweroff".into())
    } else if have("shutdown") {
        Some("shutdown".into())
    } else if have("poweroff") {
        Some("poweroff".into())
    } else {
        None
    }
}

fn python_version() -> Option<String> {
    // `python3 --version` prints to stdout on 3.x, but some builds
    // answer on stderr — and either way a success-with-empty-output
    // must report `null`, never a bogus `""`.
    let out = Command::new("python3").arg("--version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let s = if stdout.trim().is_empty() { stderr.trim() } else { stdout.trim() };
    if s.is_empty() {
        return None;
    }
    Some(s.strip_prefix("Python ").unwrap_or(s).to_string())
}

/// Collect facts about this box (design.md §8).
pub fn collect(deploy_root: &str) -> ProbeReport {
    let (gpu, driver, cuda) = nvidia_smi().unwrap_or((None, None, None));
    ProbeReport { protocol: crate::proto::wire::PROTOCOL_VERSION, gpu, driver, cuda, cores: cgroup_cores(), mem_gb: cgroup_mem_gb().map(|g| (g * 100.0).round() / 100.0), disk_free_gb: disk_free_gb(deploy_root).map(|g| (g * 100.0).round() / 100.0), python: python_version(), can_root: can_root(), shutdown_cmd: shutdown_cmd() }
}

#[cfg(test)]
mod tests {
    use super::parse_df_pk;

    #[test]
    fn parses_df_pk_output() {
        let out = "Filesystem     1024-blocks     Used Available Capacity Mounted on\n\
                   /dev/vda1        51473868 20971520  30502348      41% /\n";
        let gb = parse_df_pk(out).unwrap();
        assert!((gb - 30502348.0 / 1_048_576.0).abs() < 1e-9);
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_df_pk(""), None);
        assert_eq!(parse_df_pk("df: /x: no such\n"), None);
        assert_eq!(parse_df_pk("a b c\n"), None);
    }
}
