//! The remaining room ops: decide/roster/ls/archive/unarchive/rm
//! and the status aggregate (§17.2/§17.4/§17.5).

use crate::proto::time::rfc3339_z_now;
use crate::server::fsutil::private_dir;
use crate::server::registry::names::{self, Resolution};
use crate::server::registry::{self, valid_name};
use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use super::labels::resolve_label;
use super::store::{existing_rooms_dir, load_meta, mark, msg_numbers, resolve, room_names, rooms_dir, save_meta, unknown_room_err, unread_count, ARCHIVE_DIR};
use super::{on_roster, warn_non_member, Decision, Meta, RoomView, Suggested};

/// `room decide` (§17.2): overwrite meta.decision — latest wins.
/// No stream message: the stream retains HOW agreement was reached;
/// the decision field is WHAT it currently is. Non-members record
/// too (open trust) with the same stderr roster note as `say`.
pub fn decide(room: &str, label: &str, note: &str) -> Result<Decision> {
    let (name, dir) = resolve(room)?;
    let mut meta = load_meta(&dir)?;
    warn_non_member(&name, &meta, label, "recording the decision anyway");
    meta.decision = Some(Decision { ts: rfc3339_z_now(), author: label.to_string(), note: note.to_string() });
    // Whole-field overwrite + atomic rename: a concurrent decide
    // degrades to last-writer-wins, which is the spec's "latest
    // wins" — no lock needed.
    save_meta(&dir, &meta)?;
    mark(&dir, label, None)?;
    Ok(meta.decision.unwrap())
}

/// Roster view for `members`/`ls`: meta plus live staleness —
/// a member is stale when its project no longer shares any same-box
/// group with another project (§17.4).
pub(super) fn view(name: &str, dir: &Path, meta: &Meta, label: Option<&str>) -> RoomView {
    let stale = stale_members(meta);
    RoomView { name: name.to_string(), created_at: meta.created_at.clone(), members: meta.members.clone(), stale, clients: meta.clients.clone(), msgs: msg_numbers(dir).len(), unread_msgs: label.map(|l| unread_count(dir, l)), decision: meta.decision.clone() }
}

/// Projects grouped by shared compute (§17.4): client rows sharing
/// an ssh.host:port land in one group; every `local` row groups
/// together as "local". A group carries ≥2 distinct projects.
/// This is the RAW grouping — staleness is judged against it before
/// any room exists, so existing rooms never mask drift here.
fn binding_groups() -> Vec<Suggested> {
    let mut by_endpoint: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for cname in registry::list_clients().unwrap_or_default() {
        let Ok(c) = registry::load_client(&cname) else { continue };
        let key = if c.local {
            "local".to_string()
        } else if c.ssh.host.is_empty() {
            continue; // no endpoint yet — cannot name a shared box
        } else {
            format!("{}:{}", c.ssh.host, c.ssh.port)
        };
        by_endpoint.entry(key).or_default().push(cname);
    }
    let mut projects: Vec<(String, Vec<String>)> = Vec::new();
    for pname in registry::list_names("projects").unwrap_or_default() {
        if let Ok(p) = registry::load_project(&pname) {
            projects.push((pname, p.bind.iter().map(|b| b.client.clone()).collect()));
        }
    }
    let mut out = Vec::new();
    for (endpoint, clients) in by_endpoint {
        let mut projs: Vec<String> = projects.iter().filter(|(_, binds)| binds.iter().any(|b| clients.contains(b))).map(|(n, _)| n.clone()).collect();
        projs.sort();
        projs.dedup();
        if projs.len() >= 2 {
            out.push(Suggested { endpoint, clients, projects: projs });
        }
    }
    out
}

/// Members whose project appears in no same-box group (§17.4).
fn stale_members(meta: &Meta) -> Vec<String> {
    let groups = binding_groups();
    meta.members.iter().filter(|m| !groups.iter().any(|g| g.projects.contains(m))).cloned().collect()
}

/// The `room ls` report (§17.2/§17.4): the resolved reader label,
/// that label's member rooms (or every room under `--all`) with
/// unread counts, suggested same-box project groups, and the
/// archived room names (cold storage: names only, no cursor work).
#[derive(Serialize)]
pub struct Listings {
    pub label: String,
    pub rooms: Vec<RoomView>,
    pub suggested: Vec<Suggested>,
    pub archived: Vec<String>,
}

/// `room ls` (§17.2/§17.4): the caller's member rooms (or every
/// room under `--all`) with unread counts, plus suggested groups —
/// same-box project sets with no room yet.
pub fn list(as_flag: Option<&str>, all: bool) -> Result<Listings> {
    let label = resolve_label(as_flag)?;
    let groups = binding_groups();
    let mut views = Vec::new();
    let mut member_sets: Vec<Vec<String>> = Vec::new();
    let mut archived = Vec::new();
    if let Some(dir) = existing_rooms_dir() {
        // Cold storage lists as names only — no msgs/unread counts:
        // an archived room carries no cursor work.
        archived = archived_names(&dir);
        for name in room_names(&dir) {
            let rd = dir.join(&name);
            let meta = match load_meta(&rd) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("warning: skipping room '{name}': {e:#}");
                    continue;
                }
            };
            member_sets.push(meta.members.clone());
            // Default scope is the caller's inbox: rooms the label
            // belongs to. `--all` widens to the directory.
            if !all && !on_roster(&meta, &label) {
                continue;
            }
            views.push(view(&name, &rd, &meta, Some(&label)));
        }
    }
    // A group already has a room when some room's roster covers it
    // (superset: a room may span boxes — §17.4).
    let suggested = groups.into_iter().filter(|g| !member_sets.iter().any(|ms| g.projects.iter().all(|p| ms.contains(p)))).collect();
    Ok(Listings { label, rooms: views, suggested, archived })
}

/// Archived room names under `rooms/.archive/`, sorted — the same
/// "is a room" predicate as the live set (dirs holding meta.json).
fn archived_names(rooms: &Path) -> Vec<String> {
    room_names(&rooms.join(ARCHIVE_DIR))
}

/// `room archive` (§17): rename the room dir into
/// `rooms/.archive/` — the record is self-contained, so one atomic
/// rename is the whole operation. No lock: every op resolves its
/// dir fresh per call, so a rename mid-flight just moves where the
/// next resolution lands.
pub fn archive(room: &str) -> Result<String> {
    let (name, dir) = resolve(room)?;
    let archive = rooms_dir()?.join(ARCHIVE_DIR);
    private_dir(&archive)?;
    fs::rename(&dir, archive.join(&name)).with_context(|| format!("cannot move {} into cold storage", dir.display()))?;
    Ok(name)
}

/// `room unarchive` (§17): exact name only under `.archive/` —
/// cold storage names are typed in full, no prefix ladder. The
/// disjoint live/archive namespace makes a live namesake a hard
/// refusal, never a clobber.
pub fn unarchive(name: &str) -> Result<()> {
    valid_name(name)?;
    let rooms = rooms_dir()?;
    let src = rooms.join(ARCHIVE_DIR).join(name);
    anyhow::ensure!(src.is_dir(), "no archived room named {name:?} — `shunt room ls` lists archived rooms");
    let dest = rooms.join(name);
    anyhow::ensure!(!dest.exists(), "a live room '{name}' already exists — remove or rename it first");
    fs::rename(&src, &dest).with_context(|| format!("cannot restore {} from cold storage", src.display()))?;
    Ok(())
}

/// `room members` (§17.2): the roster view of one room.
pub fn roster(room: &str, label: Option<&str>) -> Result<RoomView> {
    let (name, dir) = resolve(room)?;
    let meta = load_meta(&dir)?;
    Ok(view(&name, &dir, &meta, label))
}

/// `room rm` (§17.4): rooms never auto-expire — the coordination
/// record is the asset — so removal is this command only. It spans
/// both namespaces: the live ladder first (exact > case-fold >
/// unique prefix), then an exact `.archive/` name — cold storage
/// names are typed in full, never prefix-matched. Returns the name
/// and whether the archived record was the one dropped, so output
/// can say which.
pub fn remove(room: &str) -> Result<(String, bool)> {
    valid_name(room)?;
    let rooms = rooms_dir()?;
    match names::resolve_room(room)? {
        Resolution::Exact(name) | Resolution::Canonical { canonical: name, .. } => {
            let dir = rooms.join(&name);
            fs::remove_dir_all(&dir).with_context(|| format!("cannot remove {}", dir.display()))?;
            Ok((name, false))
        }
        Resolution::Unknown { input, suggestions } => {
            let dir = rooms.join(ARCHIVE_DIR).join(&input);
            if dir.is_dir() {
                fs::remove_dir_all(&dir).with_context(|| format!("cannot remove {}", dir.display()))?;
                return Ok((input, true));
            }
            Err(unknown_room_err(&input, &suggestions))
        }
        Resolution::Ambiguous(cands) => anyhow::bail!("room prefix {room:?} is ambiguous: {}", cands.join(", ")),
    }
}

/// The `status --json` rooms block (§17.5): `[{name, members,
/// unread_msgs, decision}]` for the caller's label. Reporting only —
/// never marks, never fails the aggregate.
pub fn status_rooms(label: Option<&str>) -> Vec<serde_json::Value> {
    let Some(dir) = existing_rooms_dir() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for name in room_names(&dir) {
        let rd = dir.join(&name);
        let Ok(meta) = load_meta(&rd) else {
            eprintln!("warning: skipping room '{name}' (unreadable meta)");
            continue;
        };
        out.push(serde_json::json!({"name": name, "members": meta.members, "unread_msgs": label.map(|l| unread_count(&rd, l)), "decision": meta.decision}));
    }
    out
}
