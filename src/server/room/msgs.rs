//! The message stream: `say` mint+install, the loaders, and `read`
//! with its cursor marking (§17.2/§17.5).

use crate::client::queue::seq_lock;
use crate::proto::time::rfc3339_z_now;
use crate::server::fsutil::{atomic_write, private_dir};
use anyhow::Result;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use super::store::{last_read, load_meta, mark, mint_locked, msg_numbers, msg_numbers_in, resolve};
use super::{is_member, msg_keep, warn_non_member, Msg, MAX_BODY};

/// `room say` (§17.2): mint under `.seq.lock`, install
/// `msgs/NNNN.json` temp-then-rename, archive the oldest past the
/// cap, then mark the caller's label (a say marks its own message
/// read — you saw what you posted). Returns the minted N and
/// whether the author is a member (roster, or `admin` implicitly).
pub fn say(room: &str, label: &str, body: &str) -> Result<(u32, bool)> {
    anyhow::ensure!(body.len() <= MAX_BODY, "body is {} bytes; the cap is {MAX_BODY} — a coordination message carries decisions, not logs", body.len());
    let (name, dir) = resolve(room)?;
    let meta = load_meta(&dir)?;
    let member = is_member(&meta, label);
    warn_non_member(&name, &meta, label, "posting anyway, recorded member:false");
    let msg = Msg { author: label.to_string(), ts: rfc3339_z_now(), body_md: body.to_string(), member, summary_through: None };
    let n = {
        // Mint + install + archive in one hold (the §3.3
        // critical-section shape): concurrent says serialize, so no
        // two messages share a number.
        let _lock = seq_lock(&dir)?;
        // .seq.lock rides the registry's 0600 discipline too.
        let _ = fs::set_permissions(dir.join(".seq.lock"), fs::Permissions::from_mode(0o600));
        let n = mint_locked(&dir)?;
        let msgs = dir.join("msgs");
        private_dir(&msgs)?;
        atomic_write(&msgs.join(format!("{n:04}.json")), serde_json::to_string_pretty(&msg)?)?;
        // §17.4 cap: oldest-first into the dot-prefixed archive.
        let mut live = msg_numbers(&dir);
        if live.len() > msg_keep() {
            let archive = msgs.join(".archive");
            private_dir(&archive)?;
            for n in live.drain(..live.len() - msg_keep()) {
                let _ = fs::rename(msgs.join(format!("{n:04}.json")), archive.join(format!("{n:04}.json")));
            }
        }
        n
    };
    mark(&dir, label, None)?;
    Ok((n, member))
}

/// Every parseable message under one message dir. Reads happen
/// outside the lock, so a file that vanished between listing and
/// reading is a concurrent compact/cap-drain — skip it silently;
/// a parse failure or other IO error is real damage and stays loud
/// (the feedback ledger's scan discipline).
fn load_msgs_from(msgs_dir: &Path) -> Vec<(u32, Msg)> {
    let mut out = Vec::new();
    for n in msg_numbers_in(msgs_dir) {
        let p = msgs_dir.join(format!("{n:04}.json"));
        match fs::read_to_string(&p) {
            Ok(t) => match serde_json::from_str::<Msg>(&t) {
                Ok(m) => out.push((n, m)),
                Err(_) => eprintln!("warning: skipping unreadable message {}", p.display()),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => eprintln!("warning: skipping unreadable message {}", p.display()),
        }
    }
    out
}

pub(super) fn load_msgs(dir: &Path) -> Vec<(u32, Msg)> {
    load_msgs_from(&dir.join("msgs"))
}

/// The archive loader — range reads and the digest fallback reach
/// across the cap boundary.
pub(super) fn load_archived_msgs(dir: &Path) -> Vec<(u32, Msg)> {
    load_msgs_from(&dir.join("msgs").join(".archive"))
}

/// `room read` modes (§17.2): the unread tail, the full live
/// stream, the compacted catch-up view, or an audit range reaching
/// into the archive.
pub enum ReadMode {
    Unread,
    All,
    /// The newest digest plus the raw tail it did not cover — the
    /// cheap catch-up for a reader who fell behind a compacted
    /// stream.
    Digest,
    /// Inclusive bounds across live + archive (either side open).
    Range {
        from: Option<u32>,
        to: Option<u32>,
    },
}

/// `room read` (§17.2/§17.5). A label off the roster still gets the
/// stream — open trust — but the roster is named on stderr, the
/// same warning `say`/`decide` give, so a wrong-room or mis-cased
/// label is loud instead of silently reading a foreign channel.
/// Only successfully loaded, returned messages advance the cursor;
/// concurrent arrivals stay unread. `Range` is a pure audit read —
/// it never marks.
pub fn read(room: &str, label: &str, mode: ReadMode) -> Result<Vec<(u32, Msg)>> {
    let (name, dir) = resolve(room)?;
    let meta = load_meta(&dir)?;
    warn_non_member(&name, &meta, label, "returning the stream anyway");
    match mode {
        ReadMode::Range { from, to } => {
            let mut msgs = load_archived_msgs(&dir);
            msgs.extend(load_msgs(&dir));
            msgs.sort_by_key(|(n, _)| *n);
            Ok(msgs.into_iter().filter(|(n, _)| from.is_none_or(|f| *n >= f) && to.is_none_or(|t| *n <= t)).collect())
        }
        ReadMode::Digest => {
            let live = load_msgs(&dir);
            // The newest summary heads the view; when the cap has
            // archived it, only the archive still holds one.
            let summary = match live.iter().filter(|(_, m)| m.summary_through.is_some()).max_by_key(|(n, _)| *n) {
                Some((n, m)) => Some((*n, m.clone())),
                None => load_archived_msgs(&dir).into_iter().filter(|(_, m)| m.summary_through.is_some()).max_by_key(|(n, _)| *n),
            };
            let Some((n_s, s)) = summary else {
                eprintln!("note: no compaction yet — showing the full live stream");
                let through = live.last().map(|(n, _)| *n).unwrap_or(0);
                mark(&dir, label, Some(through))?;
                return Ok(live);
            };
            let covered = s.summary_through.unwrap_or(0);
            let mut out: Vec<(u32, Msg)> = live.into_iter().filter(|(n, _)| *n > covered).collect();
            if !out.iter().any(|(n, _)| *n == n_s) {
                out.push((n_s, s));
                out.sort_by_key(|(n, _)| *n);
            }
            let through = out.last().map(|(n, _)| *n).unwrap_or(0);
            mark(&dir, label, Some(through))?;
            Ok(out)
        }
        ReadMode::Unread | ReadMode::All => {
            let msgs = load_msgs(&dir);
            let mark0 = last_read(&dir, label);
            let returned: Vec<_> = if matches!(mode, ReadMode::All) { msgs } else { msgs.into_iter().filter(|(n, _)| *n > mark0).collect() };
            let through = returned.last().map(|(n, _)| *n).unwrap_or(0);
            mark(&dir, label, Some(through))?;
            Ok(returned)
        }
    }
}
