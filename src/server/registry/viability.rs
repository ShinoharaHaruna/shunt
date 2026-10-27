//! Client scheduling viability: row policy plus the shared connection-failure streak.

use super::{ClientEntry, CONN_FAIL_QUARANTINE};
use serde::Serialize;
use std::fmt;

/// A client that automatic work must not contact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum InactiveReason {
    Disabled,
    Expired,
    Quarantined { consecutive_failures: u32 },
}

impl fmt::Display for InactiveReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disabled => write!(f, "disabled"),
            Self::Expired => write!(f, "expired"),
            Self::Quarantined { consecutive_failures } => write!(f, "quarantined ({consecutive_failures} consecutive failures)"),
        }
    }
}

/// Resolve the client row's current inactive reason. Disabled and expired
/// rows outrank the failure streak: neither should be described as a dead host.
pub fn inactive_reason(client: &ClientEntry, failures: u32) -> Option<InactiveReason> {
    if client.disabled {
        return Some(InactiveReason::Disabled);
    }
    if is_expired(client) {
        return Some(InactiveReason::Expired);
    }
    (failures >= CONN_FAIL_QUARANTINE).then_some(InactiveReason::Quarantined { consecutive_failures: failures })
}

/// Epoch seconds for an `expire` string that parses as an ISO-8601
/// datetime WITH an offset (`+08:00`/`Z`). Without an offset we
/// cannot know the zone (std has no local-tz) — return None and the
/// row's raw string still reports.
pub(crate) fn expire_epoch(s: &str) -> Option<i64> {
    use toml::value::{Datetime, Offset};
    let dt = s.trim().parse::<Datetime>().ok()?;
    let d = dt.date?;
    let t = dt.time.unwrap_or(toml::value::Time { hour: 0, minute: 0, second: 0, nanosecond: 0 });
    let secs = days_from_civil(i64::from(d.year), i64::from(d.month), i64::from(d.day)) * 86400 + i64::from(t.hour) * 3600 + i64::from(t.minute) * 60 + i64::from(t.second);
    let off = match dt.offset {
        Some(Offset::Z) => 0i64,
        Some(Offset::Custom { minutes }) => i64::from(minutes) * 60,
        None => return None,
    };
    Some(secs - off)
}

/// Howard Hinnant's days-from-civil: proleptic-Gregorian days since
/// 1970-01-01. std has no datetime math, so we carry the tiny formula ourselves.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn now_epoch() -> i64 {
    crate::proto::time::unix_now() as i64
}

/// Seconds until the row's `expire` deadline; `None` when the field
/// carries no timestamp with an offset (unlimited, metered, unknown, or zoneless).
pub(crate) fn expires_left_s(client: &ClientEntry) -> Option<i64> {
    expire_epoch(&client.expire).map(|e| e - now_epoch())
}

/// Whether the row's deadline has passed. Unparseable and zoneless dates
/// remain unknown rather than being guessed in the local timezone.
pub fn is_expired(client: &ClientEntry) -> bool {
    expires_left_s(client).is_some_and(|left| left < 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn row(expire: &str) -> ClientEntry {
        ClientEntry { name: "box".into(), local: false, disabled: false, local_bin: None, bin_path: None, timeout_s: None, ssh_timeout_s: None, pre_connect: String::new(), shutdown_cmd: None, expire: expire.into(), env_setup: String::new(), shell: String::new(), endpoint_rotates: false, shared: false, exit_on_idle: false, ssh: Default::default(), qroot: "/q".into(), deploy_root: "/root".into(), facts: BTreeMap::new(), billing: None, queues: BTreeMap::new(), default_queue: None }
    }

    #[test]
    fn expired_means_a_past_offset_timestamp_only() {
        assert!(is_expired(&row("2020-01-01T00:00:00Z")));
        assert!(!is_expired(&row("2999-01-01T00:00:00Z")));
        for value in ["unlimited", "metered", "", "2020-01-01T00:00:00"] {
            assert!(!is_expired(&row(value)), "{value:?} must not read as expired");
        }
    }

    #[test]
    fn inactive_reason_orders_disabled_expiry_and_quarantine() {
        assert_eq!(inactive_reason(&row("2999-01-01T00:00:00Z"), CONN_FAIL_QUARANTINE), Some(InactiveReason::Quarantined { consecutive_failures: CONN_FAIL_QUARANTINE }));
        assert_eq!(inactive_reason(&row("2020-01-01T00:00:00Z"), CONN_FAIL_QUARANTINE), Some(InactiveReason::Expired));
        let mut disabled = row("");
        disabled.disabled = true;
        assert_eq!(inactive_reason(&disabled, CONN_FAIL_QUARANTINE), Some(InactiveReason::Disabled));
    }

    #[test]
    fn expiry_offset_parsing() {
        assert_eq!(expire_epoch("unlimited"), None);
        assert_eq!(expire_epoch("metered"), None);
        assert_eq!(expire_epoch(""), None);
        assert_eq!(expire_epoch("next friday"), None);
        assert_eq!(expire_epoch("2026-01-20T18:00:00+08:00"), expire_epoch("2026-01-20T10:00:00Z"));
        assert_eq!(expire_epoch("2026-01-20T18:00:00"), None);
        assert_eq!(expire_epoch("2026-01-20"), None);
    }
}
