//! Shared test-only helpers.
//!
//! Test scratch roots live under `target/tmp/` instead of the system temp
//! dir: residue from panicked or aborted `cargo test` runs stays out of
//! `/tmp`, and `cargo clean` wipes whatever the happy-path cleanups miss.
//! (Integration targets use `tests/common::tmp_base` — the same base.)

use std::path::PathBuf;

/// Scratch parent dir for unit tests: `<manifest>/target/tmp`. Callers
/// `create_dir_all` a tagged child under it, so it need not exist yet.
pub(crate) fn tmp_base() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("tmp")
}
