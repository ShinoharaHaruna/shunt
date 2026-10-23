//! UTC calendar timestamps shared by both halves. Two RFC 3339
//! spellings are load-bearing on disk and must not converge: `…Z`
//! inside server-side records (spool `submitted_at`, task-ledger
//! rows, events.jsonl, room files) and `…+00:00` in agent-written
//! lines (task bodies' generated-by comment, `=== start`/`=== exit`
//! task-log markers, daemon.log JSONL `ts`). The suffix-selecting
//! fns below keep every call site on its current spelling.

use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since the unix epoch.
pub fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

/// Unix seconds → broken-down UTC `(year, month, day, hour, minute,
/// second)` via Howard Hinnant's civil_from_days.
fn civil(secs: u64) -> (i64, i64, i64, i64, i64, i64) {
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let (h, m, s) = ((rem / 3600) as i64, (rem % 3600 / 60) as i64, (rem % 60) as i64);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    (y, mo, d, h, m, s)
}

/// `YYYY-MM-DDTHH:MM:SS` + `suffix`.
fn stamp(secs: u64, suffix: &str) -> String {
    let (y, mo, d, h, m, s) = civil(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}{suffix}")
}

/// `…Z` — the spelling persisted in server-side records (spool
/// `submitted_at`, task-ledger rows, room files).
pub fn rfc3339_z(secs: u64) -> String {
    stamp(secs, "Z")
}

/// `rfc3339_z` at now.
pub fn rfc3339_z_now() -> String {
    rfc3339_z(unix_now())
}

/// `…+00:00` at now — the `date -Is` spelling persisted in
/// agent-written lines (task bodies' generated-by comment,
/// `=== start`/`=== exit` task-log markers, daemon.log `ts`).
pub fn rfc3339_offset_now() -> String {
    stamp(unix_now(), "+00:00")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_epoch_and_known_date() {
        assert_eq!(rfc3339_z(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_z(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(rfc3339_offset_now().len(), rfc3339_z_now().len() + 5);
        assert!(rfc3339_offset_now().ends_with("+00:00"));
    }
}
