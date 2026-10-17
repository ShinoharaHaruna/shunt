//! `client probe` and `client bootstrap` (design.md §4.1, §7, §8).
//! Both run over raw SSH with plain shell — the remote box may not
//! have a shunt binary yet, so the probe cannot use the wire protocol.

use crate::server::registry::{self, ClientEntry};
use crate::server::transport::{shunt_bin, Transport};
use anyhow::{Context, Result};
use serde_json::json;
use std::collections::BTreeMap;

/// Shell probe script, emitted as KEY=value lines. cgroup-aware with
/// v1 fallbacks; `nproc`/`free` only as last resort (design.md §8).
/// `$1` to `sh -c` is the deploy_root for the df measurement.
const PROBE_SCRIPT: &str = r#"
g=$(nvidia-smi --query-gpu=name,driver_version --format=csv,noheader 2>/dev/null | head -n1)
echo "GPU=$g"
c=$(nvidia-smi 2>/dev/null | sed -n 's/.*CUDA Version:[[:space:]]*\([0-9.]*\).*/\1/p' | head -n1)
echo "CUDA=$c"
cores=
if [ -f /sys/fs/cgroup/cpu.max ]; then
    read quota period < /sys/fs/cgroup/cpu.max
    [ "$quota" != "max" ] && [ -n "$period" ] && cores=$((quota / period))
elif [ -f /sys/fs/cgroup/cpu/cpu.cfs_quota_us ]; then
    q=$(cat /sys/fs/cgroup/cpu/cpu.cfs_quota_us); p=$(cat /sys/fs/cgroup/cpu/cpu.cfs_period_us)
    [ "$q" -gt 0 ] && [ "$p" -gt 0 ] && cores=$((q / p))
fi
[ -z "$cores" ] && cores=$(nproc 2>/dev/null)
echo "CORES=$cores"
mem=
if [ -f /sys/fs/cgroup/memory.max ]; then
    b=$(cat /sys/fs/cgroup/memory.max); [ "$b" != "max" ] && mem=$b
elif [ -f /sys/fs/cgroup/memory/memory.limit_in_bytes ]; then
    b=$(cat /sys/fs/cgroup/memory/memory.limit_in_bytes)
    [ "$b" -lt 9223372036854771712 ] && mem=$b
fi
[ -z "$mem" ] && mem=$(free -b 2>/dev/null | awk '/^Mem:/{print $2}')
echo "MEM_BYTES=$mem"
echo "DISK_FREE_GB=$(df --output=avail -BG "$1" 2>/dev/null | tail -n1 | tr -dc '0-9')"
cr=0
[ "$(id -u 2>/dev/null)" = "0" ] && cr=1
if [ "$cr" = "0" ] && sudo -n true 2>/dev/null; then cr=1; fi
echo "CAN_ROOT=$cr"
sc=
if [ -d /run/systemd/system ] && command -v systemctl >/dev/null 2>&1; then
    sc="systemctl poweroff"
elif command -v shutdown >/dev/null 2>&1; then
    sc="shutdown"
elif command -v poweroff >/dev/null 2>&1; then
    sc="poweroff"
fi
echo "SHUTDOWN_CMD=$sc"
echo "PY=$(python3 --version 2>&1 | awk '{print $2}')"
"#;

/// Run the raw-shell probe and parse KEY=value output into facts.
pub fn probe_facts(t: &dyn Transport, client: &ClientEntry) -> Result<BTreeMap<String, serde_json::Value>> {
    let root = if client.deploy_root.is_empty() { "/" } else { client.deploy_root.as_str() };
    let out = t.exec(client, &["sh", "-c", PROBE_SCRIPT, "sh", root].iter().map(|s| s.to_string()).collect::<Vec<_>>(), None)?;
    Ok(parse_probe(&out))
}

/// Split `GPU=name, driver` and coerce numeric fields.
fn parse_probe(out: &str) -> BTreeMap<String, serde_json::Value> {
    let mut m = BTreeMap::new();
    for line in out.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let v = v.trim();
        match k {
            "GPU" if !v.is_empty() => {
                let mut it = v.splitn(2, ',');
                m.insert("gpu".into(), json!(it.next().unwrap_or("").trim()));
                if let Some(d) = it.next() {
                    m.insert("driver".into(), json!(d.trim()));
                }
            }
            "CUDA" if !v.is_empty() => {
                m.insert("cuda".into(), json!(v));
            }
            "CORES" => {
                if let Ok(n) = v.parse::<u64>() {
                    m.insert("cores".into(), json!(n));
                }
            }
            "MEM_BYTES" => {
                if let Ok(n) = v.parse::<f64>() {
                    m.insert("mem_gb".into(), json!(((n / 1e9) * 10.0).round() / 10.0));
                }
            }
            "DISK_FREE_GB" => {
                if let Ok(n) = v.parse::<f64>() {
                    m.insert("disk_free_gb".into(), json!(n));
                }
            }
            "CAN_ROOT" => {
                m.insert("can_root".into(), json!(v == "1"));
            }
            "SHUTDOWN_CMD" if !v.is_empty() => {
                m.insert("shutdown_cmd".into(), json!(v));
            }
            "PY" if !v.is_empty() => {
                m.insert("python".into(), json!(v));
            }
            _ => {}
        }
    }
    m
}

/// `shunt client probe <name>`: re-collect facts, merge into the row.
/// Roundtrip rewrites the TOML; comments are lost (documented in §4.1).
pub fn probe(t: &dyn Transport, name: &str) -> Result<()> {
    let mut entry = registry::load_client(name)?;
    let facts = probe_facts(t, &entry)?;
    entry.facts.extend(facts);
    let p = registry::save_client(&entry)?;
    println!("wrote {}", p.display());
    println!("{}", serde_json::to_string_pretty(&entry.facts)?);
    Ok(())
}

/// Generate (or reuse) the dedicated keypair at
/// `$SHUNT_HOME/keys/<name>` and point `ssh.key` at it. Called by
/// `client add` and `bootstrap` — ssh becomes registration-transparent.
pub fn ensure_key(entry: &mut ClientEntry) -> Result<()> {
    if entry.local {
        return Ok(()); // loopback needs no key
    }
    if !entry.ssh.key.is_empty() && std::path::Path::new(&entry.ssh.key).exists() {
        return Ok(()); // user-supplied or previously generated key
    }
    if !entry.ssh.key.is_empty() {
        // A set-but-missing path is a user error; surface it instead
        // of silently swapping in a fresh key.
        anyhow::bail!("ssh.key points at {} but the file is missing", entry.ssh.key);
    }
    let path = registry::client_key_path(&entry.name)?;
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    if !path.exists() {
        let st = std::process::Command::new("ssh-keygen").args(["-t", "ed25519", "-N", "", "-f", &path.display().to_string(), "-C", &format!("shunt-{}", entry.name)]).stdin(std::process::Stdio::null()).status().context("spawn ssh-keygen")?;
        if !st.success() {
            anyhow::bail!("ssh-keygen failed for {}", path.display());
        }
    }
    entry.ssh.key = path.display().to_string();
    Ok(())
}

/// Install `<key>.pub` on the box when `ssh.password` is set (AutoDL
/// default: password auth). One connection, password-only auth (no
/// pubkey offers — boxes with small MaxAuthTries drop the connection
/// after too many), pubkey appended via stdin with a `grep -qxF`
/// dedupe. Returns `true` when the key was deployed; `false` when no
/// password is configured and the `.pub` must be installed once via
/// the vendor console or a manual ssh-copy-id.
pub fn deploy_key(entry: &ClientEntry) -> Result<bool> {
    if entry.ssh.password.is_empty() {
        return Ok(false);
    }
    let have_sshpass = std::process::Command::new("sh").args(["-c", "command -v sshpass >/dev/null 2>&1"]).status().map(|s| s.success()).unwrap_or(false);
    if !have_sshpass {
        anyhow::bail!("ssh.password is set but `sshpass` is not installed — install it or deploy {}.pub manually", entry.ssh.key);
    }
    let pub_key = std::fs::read_to_string(format!("{}.pub", entry.ssh.key)).with_context(|| format!("cannot read {}.pub", entry.ssh.key))?.trim().to_string();
    let kh = registry::known_hosts_path()?.display().to_string();
    // ControlPath=none: the password-auth connection must not become
    // the mux master — later key-based execs would ride a session
    // authenticated the wrong way.
    let script = "umask 077; mkdir -p ~/.ssh; touch ~/.ssh/authorized_keys; k=$(cat); grep -qxF \"$k\" ~/.ssh/authorized_keys || echo \"$k\" >> ~/.ssh/authorized_keys";
    let mut child = std::process::Command::new("sshpass").args(["-p", &entry.ssh.password, "ssh", "-p", &entry.ssh.port.to_string(), "-o", "PreferredAuthentications=password", "-o", "PubkeyAuthentication=no", "-o", "NumberOfPasswordPrompts=1", "-o", "StrictHostKeyChecking=accept-new", "-o", &format!("UserKnownHostsFile={kh}"), "-o", "ControlPath=none", "-o", &format!("ConnectTimeout={}", entry.ssh_timeout_s.unwrap_or(15)), &format!("{}@{}", entry.ssh.user, entry.ssh.host), script]).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().context("spawn sshpass ssh")?;
    use std::io::Write;
    child.stdin.as_mut().context("stdin piped")?.write_all(pub_key.as_bytes())?;
    drop(child.stdin.take());
    let st = child.wait().context("wait sshpass")?;
    Ok(st.success())
}

/// `shunt client hostkey <name>`: drop the box's host-key entries
/// from OUR known_hosts (`ssh-keygen -R` scoped to it) and kill the
/// live mux master, so the next connect re-TOFUs a rebuilt container.
/// This is the escape hatch for `REMOTE HOST IDENTIFICATION HAS
/// CHANGED` — the user's own ~/.ssh/known_hosts is never involved.
pub fn rekey(entry: &ClientEntry) -> Result<()> {
    let kh = registry::known_hosts_path()?;
    if kh.exists() {
        let spec = if entry.ssh.port == 22 { entry.ssh.host.clone() } else { format!("[{}]:{}", entry.ssh.host, entry.ssh.port) };
        let st = std::process::Command::new("ssh-keygen").args(["-R", &spec, "-f", &kh.display().to_string()]).stdin(std::process::Stdio::null()).status().context("spawn ssh-keygen -R")?;
        if !st.success() {
            anyhow::bail!("ssh-keygen -R {spec} failed");
        }
    }
    // Kill the mux master if one is alive — it still holds a session
    // verified against the OLD key.
    let sock = crate::server::transport::cm_dir()?.join(format!("{}.sock", entry.name));
    if sock.exists() {
        let _ = std::process::Command::new("ssh").args(["-S", &sock.display().to_string(), "-O", "exit", "x"]).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
    }
    Ok(())
}

/// `shunt client env <name>`: push the row's `env_setup`/`shell` to
/// `<qroot>/env_setup` + `<qroot>/shell` on the box. Empty fields
/// remove the files so the runner falls back to bash/none.
pub fn push_env(t: &dyn Transport, name: &str) -> Result<()> {
    let e = registry::load_client(name)?;
    write_qroot_file(t, &e, "env_setup", &e.env_setup)?;
    write_qroot_file(t, &e, "shell", &e.shell)?;
    Ok(())
}

/// Write-or-remove one small file under `<qroot>` over the client's
/// transport. Empty content removes the file.
fn write_qroot_file(t: &dyn Transport, e: &ClientEntry, fname: &str, content: &str) -> Result<()> {
    let path = format!("{}/{}", e.qroot, fname);
    if content.is_empty() {
        t.exec(e, &["rm".into(), "-f".into(), path], None)?;
        return Ok(());
    }
    t.exec(e, &["mkdir".into(), "-p".into(), e.qroot.clone()], None)?;
    t.exec(e, &["sh".into(), "-c".into(), "cat > \"$0\"".into(), path], Some(content))?;
    Ok(())
}

/// `shunt client bootstrap <name>`: ensure key + try deploy, probe,
/// ship the local binary to `<qroot>/bin/shunt`, remote `init` +
/// `start`, push env_setup, merge facts.
pub fn bootstrap(t: &dyn Transport, name: &str) -> Result<()> {
    let mut entry = registry::load_client(name)?;
    if !entry.local && entry.qroot.is_empty() {
        anyhow::bail!("{}: qroot is empty — set it in the row (usually <deploy_root>/queue)", entry.name);
    }
    ensure_key(&mut entry)?;
    if deploy_key(&entry)? {
        eprintln!("key deployed: {}", entry.ssh.key);
    } else if !entry.local && entry.ssh.password.is_empty() {
        eprintln!("note: no ssh.password in the row — install {}.pub on the box once\n      (vendor console or: ssh-copy-id -i {}.pub -p {} {}@{})", entry.ssh.key, entry.ssh.key, entry.ssh.port, entry.ssh.user, entry.ssh.host);
    }
    let facts = probe_facts(t, &entry)?;
    entry.facts.extend(facts);

    let bin = shunt_bin(&entry);
    let bindir = bin.rsplit_once('/').map(|(d, _)| d).unwrap_or("").to_string();
    t.exec(&entry, &["mkdir".into(), "-p".into(), bindir.clone()], None).context("mkdir remote bin dir")?;
    let local = std::env::current_exe().context("current_exe")?;
    t.copy_to(&entry, &local, &bin).context("copy shunt binary")?;
    t.exec(&entry, &["chmod".into(), "+x".into(), bin.clone()], None)?;

    t.exec(&entry, &[bin.clone(), "init".into(), "--root".into(), entry.qroot.clone(), "--json".into()], None).context("remote shunt init")?;
    t.exec(&entry, &[bin.clone(), "start".into(), "--root".into(), entry.qroot.clone()], None).context("remote shunt start")?;

    // env_setup/shell only push when the row actually carries them;
    // an empty push would clobber a box-side edit, which is not the
    // contract bootstrap has with the registry.
    if !entry.env_setup.is_empty() || !entry.shell.is_empty() {
        if let Err(e) = push_env(t, name) {
            eprintln!("note: env_setup push failed: {e:#}");
        }
    }
    let p = registry::save_client(&entry)?;
    println!("bootstrapped {} ({})", entry.name, bin);
    println!("wrote {}", p.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_probe;

    #[test]
    fn parse_sample_output() {
        let out = "GPU=NVIDIA GeForce RTX 4090, 535.54.03\nCUDA=12.2\nCORES=14\nMEM_BYTES=60129583104\nDISK_FREE_GB=118\nCAN_ROOT=1\nSHUTDOWN_CMD=systemctl poweroff\nPY=3.10.12\n";
        let f = parse_probe(out);
        assert_eq!(f["gpu"], "NVIDIA GeForce RTX 4090");
        assert_eq!(f["driver"], "535.54.03");
        assert_eq!(f["cuda"], "12.2");
        assert_eq!(f["cores"], 14);
        assert_eq!(f["mem_gb"], 60.1);
        assert_eq!(f["disk_free_gb"], 118.0);
        assert_eq!(f["python"], "3.10.12");
        assert_eq!(f["can_root"], true);
        assert_eq!(f["shutdown_cmd"], "systemctl poweroff");
    }

    #[test]
    fn parse_cpu_only_boot() {
        let out = "GPU=\nCUDA=\nCORES=8\nMEM_BYTES=\nDISK_FREE_GB=50\nPY=\n";
        let f = parse_probe(out);
        assert!(!f.contains_key("gpu"));
        assert_eq!(f["cores"], 8);
        assert!(!f.contains_key("mem_gb"));
    }
}
