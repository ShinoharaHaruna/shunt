//! Admission/claim serialization for pending entries.

use anyhow::{Context, Result};
use fs2::FileExt;
use std::path::Path;
use std::time::{Duration, Instant};

/// Hold and release must serialize with the daemon's final gate check
/// and rename; otherwise a successful hold could race a claim and run.
pub fn admission_lock(root: &Path) -> Result<std::fs::File> {
    let path = root.join(".admission.lock");
    let lock = std::fs::OpenOptions::new().create(true).append(true).open(&path).with_context(|| format!("cannot open {}", path.display()))?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match lock.try_lock_exclusive() {
            Ok(()) => return Ok(lock),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    anyhow::bail!("timed out waiting for .admission.lock; retry after the active queue operation finishes");
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => return Err(e).context("flock .admission.lock failed"),
        }
    }
}
