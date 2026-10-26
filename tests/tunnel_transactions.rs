//! Cross-process tunnel transactions use only a networkless SSH stand-in.

use anyhow::Result;
use fs2::FileExt;
use shunt::server::registry::{ClientEntry, SshEndpoint};
use shunt::server::transport::Transport;
use shunt::server::tunnel;
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Fake;
impl Transport for Fake {
    fn exec(&self, _: &ClientEntry, argv: &[String], _: Option<&str>) -> Result<String> {
        if let Ok(gate) = std::env::var("TUNNEL_ENV_GATE") {
            fs::write(format!("{gate}.ready"), "ready")?;
            let mut byte = [0];
            File::open(gate)?.read_exact(&mut byte)?;
        }
        let out = Command::new("/bin/sh").args(&argv[1..]).output()?;
        anyhow::ensure!(out.status.success(), "mock env edit failed: {out:?}");
        Ok(String::new())
    }
    fn copy_to(&self, _: &ClientEntry, _: &Path, _: &str) -> Result<()> {
        anyhow::bail!("unexpected copy")
    }
}

fn client(home: &Path) -> ClientEntry {
    ClientEntry { name: "remote".into(), local: false, disabled: false, local_bin: None, bin_path: None, queues: Default::default(), default_queue: None, timeout_s: None, ssh_timeout_s: None, pre_connect: String::new(), shutdown_cmd: None, expire: String::new(), env_setup: String::new(), shell: String::new(), endpoint_rotates: false, shared: false, exit_on_idle: false, ssh: SshEndpoint { host: "example.invalid".into(), user: "test".into(), port: 22, key: String::new(), password: String::new(), options: Vec::new() }, qroot: home.join("queue").display().to_string(), deploy_root: "/".into(), facts: BTreeMap::new(), billing: None }
}

#[test]
fn transaction_worker() -> Result<()> {
    let Ok(op) = std::env::var("TUNNEL_TEST_OP") else { return Ok(()) };
    let home = PathBuf::from(std::env::var_os("SHUNT_HOME").unwrap());
    let c = client(&home);
    fs::write(std::env::var("TUNNEL_STARTED")?, "started")?;
    match op.as_str() {
        "up" | "up-other" => {
            let port = if op == "up" { 1080 } else { 2080 };
            let pid = tunnel::up(&Fake, &c, Some(port))?;
            fs::write(std::env::var("TUNNEL_RESULT")?, pid.to_string())?;
        }
        "down" => tunnel::down(&Fake, &c)?,
        "heal" => {
            tunnel::heal(&Fake, &c)?;
        }
        "status" => {
            tunnel::status(&c.name)?;
        }
        "recorded" => {
            tunnel::recorded(&c.name)?;
        }
        _ => anyhow::bail!("unknown operation"),
    }
    Ok(())
}

struct Worker {
    child: Child,
    log: PathBuf,
}
impl Worker {
    fn finish(&mut self, succeeds: bool) {
        wait_until(|| self.child.try_wait().unwrap().is_some(), "worker exit");
        assert_eq!(self.child.wait().unwrap().success(), succeeds, "{}", fs::read_to_string(&self.log).unwrap_or_default());
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Fixture {
    home: PathBuf,
}
impl Fixture {
    fn new(tag: &str) -> Result<Self> {
        // Detached mock tunnels are adopted here, not leaked as PID-1 zombies
        // when a worker exits. Reap only this fixture's recorded mock PIDs.
        static SUBREAPER: std::sync::Once = std::sync::Once::new();
        SUBREAPER.call_once(|| assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) }, 0));
        let home = std::env::temp_dir().join(format!("shunt-tunnel-tx-{tag}-{}", std::process::id()));
        fs::create_dir_all(home.join("bin"))?;
        fs::create_dir_all(home.join("tunnels"))?;
        let f = Self { home };
        f.fifo("ssh-hold")?;
        // exec preserves the PID and argv needles; the FIFO blocks without grandchildren.
        fs::write(f.home.join("bin/ssh"), "#!/bin/bash\nprintf '%s\\n' \"$$\" >> \"$SHUNT_HOME/attempts\"\n[ -f \"$SHUNT_HOME/ssh-fail\" ] && exit 1\nexec -a ssh /bin/bash -c 'while read -r _; do :; done <> \"$SHUNT_HOME/ssh-hold\"' -- \"$@\"\n")?;
        fs::set_permissions(f.home.join("bin/ssh"), fs::Permissions::from_mode(0o700))?;
        Ok(f)
    }
    fn fifo(&self, name: &str) -> Result<()> {
        anyhow::ensure!(Command::new("/usr/bin/mkfifo").arg(self.home.join(name)).status()?.success(), "mkfifo");
        Ok(())
    }
    fn spawn(&self, op: &str, label: &str, gated: bool) -> Result<Worker> {
        let mut cmd = Command::new(std::env::current_exe()?);
        cmd.args(["--exact", "transaction_worker", "--nocapture"]).env("SHUNT_HOME", &self.home).env("SHUNT_STATE", self.home.join("state")).env("SHUNT_ROOT", self.home.join("queue")).env("PATH", format!("{}:/usr/bin:/bin", self.home.join("bin").display())).env("TUNNEL_TEST_OP", op).env("TUNNEL_RESULT", self.home.join(format!("{label}.result"))).env("TUNNEL_STARTED", self.home.join(format!("{label}.started"))).env_remove("TUNNEL_ENV_GATE").stdin(Stdio::null());
        if gated {
            cmd.env("TUNNEL_ENV_GATE", self.home.join(label));
        }
        let log = self.home.join(format!("{label}.log"));
        let out = OpenOptions::new().create(true).truncate(true).write(true).mode(0o600).open(&log)?;
        cmd.stderr(out.try_clone()?).stdout(out);
        let worker = Worker { child: cmd.spawn()?, log };
        wait_until(|| self.home.join(format!("{label}.started")).exists(), "worker start");
        Ok(worker)
    }
    fn run(&self, op: &str, label: &str) -> Result<()> {
        self.spawn(op, label, false)?.finish(true);
        Ok(())
    }
    fn lock(&self) -> Result<File> {
        Ok(OpenOptions::new().create(true).append(true).mode(0o600).open(self.home.join("tunnels/.remote.lock"))?)
    }
    fn assert_transaction_locked(&self, label: &str) -> Result<()> {
        wait_until(|| self.home.join(format!("{label}.ready")).exists(), "env transaction barrier");
        let lock = self.lock()?;
        let result = lock.try_lock_exclusive();
        assert!(matches!(result, Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock), "transaction released its lock before env edit: {result:?}");
        Ok(())
    }
    fn release(&self, label: &str) -> Result<()> {
        use std::io::Write;
        OpenOptions::new().write(true).open(self.home.join(label))?.write_all(b"x")?;
        Ok(())
    }
    fn pids(&self) -> Vec<u32> {
        fs::read_to_string(self.home.join("attempts")).unwrap_or_default().lines().map(|s| s.parse().unwrap()).collect()
    }
    fn assert_down(&self) {
        assert!(!self.home.join("tunnels/remote.pid").exists());
        assert!(!self.home.join("tunnels/remote.port").exists());
        assert!(!fs::read_to_string(self.home.join("queue/env")).unwrap_or_default().contains("ALL_PROXY"));
        wait_until(|| self.pids().iter().all(|p| !mock_alive(*p, &self.home)), "mock tunnel exit");
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for pid in self.pids() {
            if mock_alive(pid, &self.home) {
                let _ = Command::new("/bin/kill").args(["-KILL", "--", &format!("-{pid}")]).status();
                wait_until(|| !mock_alive(pid, &self.home), "mock cleanup");
            }
            let mut status = 0;
            let rc = unsafe { libc::waitpid(pid as i32, &mut status, 0) };
            assert!(rc == pid as i32 || std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD), "reap mock {pid}");
        }
        let _ = fs::remove_dir_all(&self.home);
    }
}
fn mock_alive(pid: u32, home: &Path) -> bool {
    let cl = fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    cl.starts_with(b"ssh\0") && String::from_utf8_lossy(&cl).contains(home.to_str().unwrap())
}
fn wait_until(mut condition: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn competing_up_is_idempotent_through_env_commit() -> Result<()> {
    let f = Fixture::new("up")?;
    f.fifo("first")?;
    let mut first = f.spawn("up", "first", true)?;
    f.assert_transaction_locked("first")?;
    let mut second = f.spawn("up-other", "second", false)?;
    f.release("first")?;
    first.finish(true);
    second.finish(true);
    assert_eq!(f.pids().len(), 1, "idempotent up spawned another tunnel");
    assert_eq!(fs::read_to_string(f.home.join("first.result"))?, fs::read_to_string(f.home.join("second.result"))?, "idempotent up returned a different PID");
    assert_eq!(fs::read_to_string(f.home.join("tunnels/remote.port"))?, "1080\n");
    assert_eq!(fs::read_to_string(f.home.join("tunnels/remote.pid"))?.trim().parse::<u32>()?, f.pids()[0]);
    f.run("down", "cleanup")?;
    f.assert_down();
    Ok(())
}

#[test]
fn down_then_competing_up_preserves_new_proxy() -> Result<()> {
    let f = Fixture::new("down-up")?;
    f.run("up", "setup")?;
    f.fifo("down")?;
    let mut down = f.spawn("down", "down", true)?;
    f.assert_transaction_locked("down")?;
    let mut up = f.spawn("up-other", "up", false)?;
    f.release("down")?;
    down.finish(true);
    up.finish(true);
    assert_eq!(f.pids().len(), 2);
    assert_eq!(fs::read_to_string(f.home.join("tunnels/remote.port"))?, "2080\n");
    assert!(fs::read_to_string(f.home.join("queue/env"))?.contains(":2080"));
    f.run("down", "cleanup")?;
    f.assert_down();
    Ok(())
}

#[test]
fn heal_then_competing_down_does_not_restore_proxy() -> Result<()> {
    for tag in ["reconnect", "healthy"] {
        let f = Fixture::new(tag)?;
        fs::write(f.home.join("tunnels/remote.port"), "2080\n")?;
        if tag == "healthy" {
            f.run("up-other", "setup")?;
        }
        f.fifo("heal")?;
        let mut heal = f.spawn("heal", "heal", true)?;
        f.assert_transaction_locked("heal")?;
        let mut down = f.spawn("down", "down", false)?;
        f.release("heal")?;
        heal.finish(true);
        down.finish(true);
        assert_eq!(f.pids().len(), 1);
        f.assert_down();
    }
    Ok(())
}

#[test]
fn failed_heal_cannot_strip_competing_ups_proxy() -> Result<()> {
    let f = Fixture::new("strip-up")?;
    fs::write(f.home.join("tunnels/remote.port"), "1080\n")?;
    fs::write(f.home.join("ssh-fail"), "fail")?;
    f.fifo("heal")?;
    let mut heal = f.spawn("heal", "heal", true)?;
    f.assert_transaction_locked("heal")?;
    fs::remove_file(f.home.join("ssh-fail"))?;
    let mut up = f.spawn("up-other", "up", false)?;
    f.release("heal")?;
    heal.finish(true);
    up.finish(true);
    assert_eq!(f.pids().len(), 2);
    assert!(fs::read_to_string(f.home.join("queue/env"))?.contains(":2080"));
    f.run("down", "cleanup")?;
    f.assert_down();
    Ok(())
}

#[test]
fn state_write_failure_does_not_orphan_new_tunnel() -> Result<()> {
    for part in ["port", "pid"] {
        let f = Fixture::new(&format!("write-{part}-failure"))?;
        let broken = f.home.join(format!("tunnels/remote.{part}"));
        fs::create_dir(&broken)?;
        f.spawn("up", "up", false)?.finish(false);
        assert_eq!(f.pids().len(), 1);
        assert!(!mock_alive(f.pids()[0], &f.home), "state write failure orphaned SSH");
        if part == "port" {
            assert!(!f.home.join("tunnels/remote.pid").exists());
        }
        fs::remove_dir(broken)?;
        f.run("up", "retry")?;
        f.run("down", "cleanup")?;
        f.assert_down();
    }
    Ok(())
}

#[test]
fn setup_failure_is_reaped_and_command_identity_is_preserved() -> Result<()> {
    let f = Fixture::new("identity")?;
    fs::write(f.home.join("ssh-fail"), "fail")?;
    f.spawn("up", "failed", false)?.finish(false);
    assert!(!f.home.join("tunnels/remote.pid").exists());
    assert!(!mock_alive(f.pids()[0], &f.home));
    fs::remove_file(f.home.join("ssh-fail"))?;
    f.run("up-other", "setup")?;
    let foreign = *f.pids().last().unwrap();
    fs::write(f.home.join("tunnels/remote.port"), "1080\n")?;
    f.run("down", "down")?;
    assert!(mock_alive(foreign, &f.home), "down signalled a mismatched SSH command");
    Ok(())
}

#[test]
fn pid_only_failed_heal_remains_eligible_for_retry() -> Result<()> {
    let f = Fixture::new("pid-only")?;
    fs::write(f.home.join("tunnels/remote.pid"), "99999999\n")?;
    fs::write(f.home.join("ssh-fail"), "fail")?;
    for attempt in 1..=2 {
        f.run("heal", &format!("heal-{attempt}"))?;
        assert!(f.home.join("tunnels/remote.pid").exists() || f.home.join("tunnels/remote.port").exists(), "failed heal lost the only recorded-tunnel marker");
        assert_eq!(f.pids().len(), attempt);
    }
    Ok(())
}
