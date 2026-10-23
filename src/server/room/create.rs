//! `room create` (§17.2): roster canonicalization and exclusive
//! directory init.

use crate::proto::time::rfc3339_z_now;
use crate::server::fsutil::private_dir;
use crate::server::registry::names::{self, Resolution};
use crate::server::registry::{self, valid_name};
use anyhow::{Context, Result};

use super::store::{initialize_room, rooms_dir, save_meta, ARCHIVE_DIR};
use super::{is_admin, Meta};

/// `room create` (§17.2). Members are project names; clients are
/// annotation context only.
pub fn create(name: &str, members: &[String], clients: &[String]) -> Result<()> {
    valid_name(name)?;
    // Dot-prefixed dirs are shunt-internal (`.archive` is the
    // cold-storage namespace) and invisible to the live candidate
    // set — a leading-dot room would resolve nowhere.
    anyhow::ensure!(!name.starts_with('.'), "room names can't start with '.' — {ARCHIVE_DIR}/ is the cold-storage namespace, and dot-prefixed dirs are invisible to `room ls` and resolution");
    anyhow::ensure!(!members.is_empty(), "room '{name}' needs --members p1,p2 — a room coordinates the projects sharing compute");
    // Members canonicalize against registered projects through the
    // name middleware — the roster then stores the spelling
    // identities resolve to. A non-registered name still lands
    // (membership is declared data); it is noted below along with
    // any did-you-mean suggestions the middleware found.
    let mut ms: Vec<String> = Vec::new();
    let mut member_hints: Vec<(String, Vec<String>)> = Vec::new();
    for m in members {
        let m = m.trim();
        valid_name(m).with_context(|| format!("bad member name {m:?} — members are project names"))?;
        anyhow::ensure!(!is_admin(m), "member {m:?} is the reserved operator identity (§17.3) — `admin` can never join a roster");
        let m = match names::resolve_project(m)? {
            Resolution::Exact(p) => p,
            Resolution::Canonical { canonical, typed } => {
                eprintln!("note: member '{typed}' → project '{canonical}'");
                canonical
            }
            Resolution::Ambiguous(projects) => anyhow::bail!("member {m:?} is ambiguous: registered projects {} differ only by case — rename one", projects.join(", ")),
            Resolution::Unknown { suggestions, .. } => {
                member_hints.push((m.to_string(), suggestions));
                m.to_string()
            }
        };
        if !ms.iter().any(|x| x == &m) {
            ms.push(m);
        }
    }
    let mut cs: Vec<String> = Vec::new();
    let mut client_hints: Vec<(String, Vec<String>)> = Vec::new();
    for c in clients {
        let c = c.trim();
        valid_name(c).with_context(|| format!("bad client name {c:?}"))?;
        let c = match names::resolve_client(c)? {
            Resolution::Exact(n) => n,
            Resolution::Canonical { canonical, typed } => {
                eprintln!("note: client '{typed}' → client '{canonical}'");
                canonical
            }
            Resolution::Ambiguous(cands) => anyhow::bail!("client {c:?} is ambiguous — registered names differ only by case: {}", cands.join(", ")),
            Resolution::Unknown { suggestions, .. } => {
                client_hints.push((c.to_string(), suggestions));
                c.to_string()
            }
        };
        if !cs.iter().any(|x| x == &c) {
            cs.push(c);
        }
    }
    let rooms = rooms_dir()?;
    let dir = rooms.join(name);
    // Live and archived names share one namespace (§17): an
    // archived namesake claims the name already. Checked before
    // the exclusive mkdir — the disjoint invariant means this can
    // only fire when no live dir exists, so the error names
    // `unarchive`/`rm` rather than the generic "already exists".
    anyhow::ensure!(!rooms.join(ARCHIVE_DIR).join(name).is_dir(), "an archived room '{name}' exists — `shunt room unarchive {name}` restores it, or `shunt room rm {name}` removes it");
    initialize_room(&dir, |dir| {
        private_dir(&dir.join("msgs"))?;
        private_dir(&dir.join("readers"))?;
        save_meta(dir, &Meta { created_at: rfc3339_z_now(), members: ms.clone(), clients: cs.clone(), decision: None, compacted_through: None })
    })?;
    // Membership is declared data, not gated — but a member that is
    // not a registered project is usually a typo, so say so (the
    // same for the client annotations).
    for m in &ms {
        match member_hints.iter().find(|(n, _)| n == m) {
            Some((_, sugg)) => eprintln!("note: member '{m}' is not a registered project{} — `project ls` lists what exists", names::did_you_mean(sugg)),
            None if registry::load_project(m).is_err() => eprintln!("note: member '{m}' is not a registered project — `project ls` lists what exists"),
            None => {}
        }
    }
    for c in &cs {
        match client_hints.iter().find(|(n, _)| n == c) {
            Some((_, sugg)) => eprintln!("note: client '{c}' is not a registered client{} — `client ls` lists what exists", names::did_you_mean(sugg)),
            None if registry::load_client(c).is_err() => eprintln!("note: client '{c}' is not a registered client — `client ls` lists what exists"),
            None => {}
        }
    }
    Ok(())
}
