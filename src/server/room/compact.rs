//! Stream compaction (§17.4): the plan/commit pair — an agent
//! digests the covered prefix and commits it as an in-stream
//! summary message.

use crate::client::queue::seq_lock;
use crate::proto::time::rfc3339_z_now;
use crate::server::fsutil::{atomic_write, private_dir};
use anyhow::Result;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use super::msgs::load_msgs;
use super::store::{load_meta, mark, mint_locked, msg_numbers, resolve, save_meta};
use super::{compact_min, on_roster, warn_non_member, Msg, Reader, MAX_BODY};

/// The packet `room compact` (plan phase) hands an agent to feed
/// its LLM: the split point to pass back as `--through`, and every
/// covered message with its number.
pub struct CompactPlan {
    pub through: u32,
    pub msg_count: usize,
    pub total_bytes: usize,
    pub messages: Vec<(u32, Msg)>,
}

/// Pick the eligible index that best cuts the stream at `ratio` of
/// total body bytes — readability is a byte metric, so the split is
/// byte-weighted, not count-weighted. Ties take the earlier index.
pub(super) fn split_index(bytes: &[usize], ratio: f64) -> usize {
    let target = ratio * bytes.iter().sum::<usize>() as f64;
    let (mut best, mut best_d, mut cum) = (0usize, f64::INFINITY, 0f64);
    for (i, b) in bytes.iter().enumerate() {
        cum += *b as f64;
        let d = (cum - target).abs();
        if d < best_d {
            best = i;
            best_d = d;
        }
    }
    best
}

/// `room compact` plan phase: pick the byte-weighted split and
/// return the covered messages. Pure audit read — nothing is
/// written, nothing is marked; the label only drives the
/// non-member warning, since the stream is content being read.
pub fn compact_plan(room: &str, label: &str, ratio: f64) -> Result<CompactPlan> {
    anyhow::ensure!(ratio > 0.0 && ratio < 1.0, "--ratio must be between 0 and 1 exclusive — 0.8 digests the oldest ~80% of eligible bytes");
    let (name, dir) = resolve(room)?;
    let meta = load_meta(&dir)?;
    warn_non_member(&name, &meta, label, "reading the stream anyway");
    let ratchet = meta.compacted_through.unwrap_or(0);
    // Digests are never re-digested; `n > ratchet` is defensive —
    // the ratchet normally tracks exactly what the move archived.
    let eligible: Vec<(u32, Msg)> = load_msgs(&dir).into_iter().filter(|(n, m)| m.summary_through.is_none() && *n > ratchet).collect();
    let min = compact_min();
    anyhow::ensure!(eligible.len() >= min, "only {} non-summary live messages — compaction pays off on long streams (min {min}); `room compact --through M (--body|--body-file|-)` forces it manually", eligible.len());
    let k = split_index(&eligible.iter().map(|(_, m)| m.body_md.len()).collect::<Vec<_>>(), ratio);
    let covered: Vec<(u32, Msg)> = eligible.into_iter().take(k + 1).collect();
    Ok(CompactPlan { through: covered.last().map(|(n, _)| *n).unwrap_or(0), total_bytes: covered.iter().map(|(_, m)| m.body_md.len()).sum(), msg_count: covered.len(), messages: covered })
}

/// Reader labels whose high-water mark sits below `through` — the
/// commit warning names them so the digester knows whose next
/// `read` lands on the summary instead of the originals.
fn unread_reader_labels(dir: &Path, through: u32) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir(dir.join("readers")) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Some(label) = name.strip_suffix(".json").filter(|_| !name.starts_with('.')) else {
                continue;
            };
            let pos = fs::read_to_string(e.path()).ok().and_then(|t| serde_json::from_str::<Reader>(&t).ok()).map(|r| r.last_read_n).unwrap_or(0);
            if pos < through {
                out.push(label.to_string());
            }
        }
    }
    out.sort();
    out
}

/// `room compact --through N` commit phase: archive the covered
/// originals and mint the agent-written digest as an in-stream
/// message. Returns the summary's N and the caller's roster flag.
/// Ordering inside the lock (move → mint → write → meta) is
/// deliberate: a mid-move failure looks exactly like cap-archival
/// and re-running the same command converges, while a failure
/// after the digest write is caught by the idempotency check on
/// the next attempt.
pub fn compact_commit(room: &str, label: &str, through: u32, body: &str) -> Result<(u32, bool)> {
    anyhow::ensure!(body.len() <= MAX_BODY, "body is {} bytes; the cap is {MAX_BODY} — a coordination message carries decisions, not logs", body.len());
    anyhow::ensure!(through > 0, "--through 0 covers no messages — the plan's split point is at least the first eligible message");
    let (name, dir) = resolve(room)?;
    let meta = load_meta(&dir)?;
    let member = on_roster(&meta, label);
    anyhow::ensure!(through > meta.compacted_through.unwrap_or(0), "messages ≤{through} are already covered by a previous compaction — choose a later --through");
    warn_non_member(&name, &meta, label, "posting anyway, recorded member:false");
    if msg_numbers(&dir).last().is_some_and(|&max| through >= max) {
        eprintln!("warning: --through covers the newest live message — recent posts may not have been seen by all members");
    }
    let unread = unread_reader_labels(&dir, through);
    if !unread.is_empty() {
        eprintln!("warning: readers who haven't seen the covered range: {} — their next `read` shows this digest instead of the originals", unread.join(", "));
    }
    let n_s = {
        let _lock = seq_lock(&dir)?;
        // .seq.lock rides the registry's 0600 discipline too.
        let _ = fs::set_permissions(dir.join(".seq.lock"), fs::Permissions::from_mode(0o600));
        let live = load_msgs(&dir);
        match live.iter().find(|(_, m)| m.summary_through == Some(through)) {
            // A live digest at exactly this `through` is a committed
            // compact whose caller died after the write — land the
            // ratchet and return its n; never mint a duplicate.
            Some((n_s, _)) => {
                let n_s = *n_s;
                let mut meta = load_meta(&dir)?;
                if meta.compacted_through.unwrap_or(0) < through {
                    meta.compacted_through = Some(through);
                    save_meta(&dir, &meta)?;
                }
                n_s
            }
            None => {
                // Re-check on fresh meta under the lock: a compact
                // that landed between the pre-lock read and this
                // hold would otherwise be double-covered.
                let mut meta = load_meta(&dir)?;
                anyhow::ensure!(through > meta.compacted_through.unwrap_or(0), "messages ≤{through} are already covered by a previous compaction — choose a later --through");
                let msgs = dir.join("msgs");
                for (n, m) in &live {
                    if *n > through {
                        break;
                    }
                    // Digests stay live — exempt from the compact
                    // move (not from the cap drain); unparseable
                    // files never reach this list, and the loader's
                    // warning already named them.
                    if m.summary_through.is_some() {
                        continue;
                    }
                    let archive = msgs.join(".archive");
                    private_dir(&archive)?;
                    fs::rename(msgs.join(format!("{n:04}.json")), archive.join(format!("{n:04}.json")))?;
                }
                private_dir(&msgs)?;
                let n_s = mint_locked(&dir)?;
                let summary = Msg { author: label.to_string(), ts: rfc3339_z_now(), body_md: body.to_string(), member, summary_through: Some(through) };
                atomic_write(&msgs.join(format!("{n_s:04}.json")), serde_json::to_string_pretty(&summary)?)?;
                meta.compacted_through = Some(through);
                save_meta(&dir, &meta)?;
                n_s
            }
        }
    };
    // mark() takes .seq.lock itself — calling it inside the hold
    // deadlocks (same shape as say()).
    mark(&dir, label, None)?;
    Ok((n_s, member))
}
