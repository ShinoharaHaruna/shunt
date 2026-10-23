//! SIGUSR2 handoff re-exec (design.md §3.5a): the queue daemon and
//! the dispatcher replace their own process image in place — same
//! pid, argv, environment, and held fds preserved — so a
//! stage-then-mv binary swap takes effect without draining or
//! killing in-flight work.

use std::ffi::OsString;
use std::io;
use std::path::PathBuf;

/// Env marker a handoff-spawned image sees (and consumes) so its
/// startup can log `handoff: true` instead of reading as a cold boot.
pub const HANDOFF_ENV: &str = "SHUNT_HANDOFF";

/// `/proc/self/exe` resolved to the canonical install path. After a
/// stage-then-mv upgrade the link reads `<path> (deleted)` — the
/// deleted inode is the OLD image; stripping the suffix names the
/// path where the new binary now sits.
pub fn self_exe() -> io::Result<PathBuf> {
    let p = std::fs::read_link("/proc/self/exe")?;
    let s = p.to_string_lossy();
    Ok(PathBuf::from(s.strip_suffix(" (deleted)").unwrap_or(&s)))
}

/// Block SIGUSR2 process-wide. The signal stays blocked across
/// `execve`, so a USR2 arriving in the exec → handler-install gap
/// pends for the new image instead of default-killing it (§3.5a).
fn block_usr2() {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGUSR2);
        libc::sigprocmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
    }
}

/// Undo `block_usr2` in the new image once its USR2 handler exists;
/// a signal pended across the exec then delivers and re-fires one
/// harmless convergent exec. Unconditional is fine — on a cold boot
/// nothing was ever blocked.
pub fn unblock_usr2() {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGUSR2);
        libc::sigprocmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
    }
}

/// Re-exec this process image with `args` (original argv minus
/// argv[0]) against the `self_exe` path. Success never returns — the
/// caller gets the exec error to log, and must keep running the old
/// image (§3.5a: never fall back to the TERM drain).
pub fn exec_self(args: &[OsString]) -> io::Result<std::convert::Infallible> {
    use std::os::unix::process::CommandExt;
    let exe = self_exe()?;
    block_usr2();
    std::env::set_var(HANDOFF_ENV, "1");
    let err = std::process::Command::new(&exe).args(args).exec();
    // exec failed: restore the mask or every later USR2 pends
    // forever and `restart` goes dead.
    unblock_usr2();
    Err(err)
}
