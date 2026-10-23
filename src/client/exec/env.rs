//! The queue-level `env` file: literal KEY=value lines injected
//! into every spawned task's environment (design.md §3.12).

use std::collections::BTreeMap;
use std::path::Path;

/// Read the dotenv-style `env` file under the queue root
/// (`KEY=value` per line, `#` comments and blanks skipped). The
/// runner consumes `WORKDIR`, `PATH_PREPEND` and `TASK_THREADS` itself
/// and injects any remaining keys into the task's environment.
///
/// Values are LITERAL: there is no shell between the file and
/// `Command::env`, so no quote stripping, no `$VAR` expansion, no
/// escape processing. `K="a b"` yields a value that *contains* the
/// quote characters; write `K=a b` instead — embedded spaces are
/// preserved verbatim. The first `=` splits key/value; the KEY is
/// whitespace-trimmed (keys can't contain spaces anyway) but the
/// value is kept strictly literal — `K= padded ` passes the spaces
/// through to `Command::env` untouched.
pub fn read_env_file(root: &Path) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    if let Ok(text) = std::fs::read_to_string(root.join("env")) {
        for line in text.lines() {
            // trim_start only: leading whitespace may indent a `#`
            // comment or blank line, but a trim_end would eat
            // meaningful trailing spaces in the value (§3.12 literal).
            let line = line.trim_start();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                let k = k.trim();
                // `Command::env` panics on an empty key or on NUL in
                // key/value — a `=foo` or NUL-bearing line must be
                // dropped here, not crash the daemon mid-dispatch
                // (a claimed entry that panics dispatch requeues as
                // an orphan and crash-loops forever, design.md §3.6).
                if !k.is_empty() && !k.contains('\0') && !v.contains('\0') {
                    map.insert(k.to_string(), v.to_string());
                }
            }
        }
    }
    map
}
