//! Coordination rooms (design.md §17): a long-lived named group
//! chat whose members are the PROJECTS sharing compute — the side
//! channel the queue deliberately withholds. Everything is
//! server-local under `$SHUNT_HOME/rooms/<name>/`: no wire op, no
//! box-side state, no interaction with scheduling or the
//! idle-shutdown gate. `meta.json` holds the roster + the current
//! decision; `msgs/NNNN.json` is one linear stream minted under
//! `.seq.lock`; `readers/<label>.json` is each identity's
//! high-water mark.

use crate::agent::queue::{seq_lock, unix_now};
use crate::proto::task::sanitize_slug;
use crate::server::fsutil::{private_dir, write_private};
use crate::server::registry::{self, shunt_home, valid_name};
use crate::server::spool::iso_utc;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// body_md hard cap (§17.1): a coordination message has no business
/// carrying logs — oversized bodies are rejected before any write.
pub const MAX_BODY: usize = 16 * 1024;

/// Live stream is a 4-digit namespace like the queue's (§17.1
/// defers to the §3.2 rules). Unlike the queue there is no
/// priority band, so the counter starts at 0 and `0001` is the
/// first message.
const SEQ_INIT: u32 = 0;
const SEQ_MAX: u32 = 9999;

/// Live-stream cap (§17.4): past this many `msgs/NNNN.json` the
/// oldest move to `msgs/.archive/` — dot-prefixed, so `read`/`ls`
/// never see them. `SHUNT_ROOM_KEEP` overrides (tests).
fn msg_keep() -> usize {
    std::env::var("SHUNT_ROOM_KEEP").ok().and_then(|s| s.parse().ok()).unwrap_or(500)
}

/// readers/ file cap (§17.4): oldest-mtime evicted past it — a
/// label gone quiet loses only its read positions, never data.
/// `SHUNT_ROOM_READERS_MAX` overrides (tests).
fn readers_max() -> usize {
    std::env::var("SHUNT_ROOM_READERS_MAX").ok().and_then(|s| s.parse().ok()).unwrap_or(64)
}

/// Room record: `meta.json` (§17.1). `members` are project names —
/// the shared vocabulary — while `clients` is annotation context
/// (which boxes the room coordinates), never a gate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub created_at: String,
    #[serde(default)]
    pub members: Vec<String>,
    #[serde(default)]
    pub clients: Vec<String>,
    /// The current agreed state; `decide` overwrites it, latest wins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<Decision>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    pub ts: String,
    pub author: String,
    pub note: String,
}

/// One stream entry: `msgs/NNNN.json`. `member: false` marks a
/// post from outside the roster — allowed (open trust), but loud.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Msg {
    pub author: String,
    pub ts: String,
    pub body_md: String,
    pub member: bool,
}

/// `readers/<label>.json`: the label's high-water mark (§17.5).
#[derive(Debug, Default, Serialize, Deserialize)]
struct Reader {
    #[serde(default)]
    last_read_n: u32,
}

/// One suggested room: projects whose bindings land on the same box
/// (client rows sharing an ssh.host:port; local rows group together)
/// with ≥2 distinct projects (§17.4). Computed live from bindings —
/// zero stored state, always accurate.
#[derive(Debug, Serialize)]
pub struct Suggested {
    /// `host:port` for ssh rows, "local" for the loopback group.
    pub endpoint: String,
    pub clients: Vec<String>,
    pub projects: Vec<String>,
}

/// What `ls` reports per room.
#[derive(Debug, Serialize)]
pub struct RoomView {
    pub name: String,
    pub created_at: String,
    pub members: Vec<String>,
    /// Members whose project no longer shares any same-box group —
    /// drift is flagged, never hidden (§17.4).
    pub stale: Vec<String>,
    pub clients: Vec<String>,
    pub msgs: usize,
    /// Unread for the caller's label; `None` when no label resolved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unread_msgs: Option<usize>,
    pub decision: Option<Decision>,
}

/// The caller's `--as` label: author of messages AND key of the
/// readers/ cursor — you read under the name you speak under
/// (§17.3). Resolution: explicit `--as` > `$USER`; the value is
/// sanitized like a §3.3 slug, then must also be a valid registry
/// name (it becomes `readers/<label>.json` — a `..` must not
/// escape). `Ok(None)` = neither source exists.
pub(crate) fn resolve_label(as_flag: Option<&str>) -> Result<Option<String>> {
    let raw = match as_flag {
        Some(s) => s.to_string(),
        None => match std::env::var("USER") {
            Ok(u) if !u.trim().is_empty() => u,
            _ => return Ok(None),
        },
    };
    let label = sanitize_slug(&raw);
    valid_name(&label).with_context(|| format!("bad --as label {raw:?} (sanitized to {label:?}): use letters, digits, '_', '.', '-' — your project name is the convention"))?;
    Ok(Some(label))
}

/// A marking op's label, or the error naming how to supply one.
pub(crate) fn required_label(as_flag: Option<&str>) -> Result<String> {
    resolve_label(as_flag)?.context("no identity to post/read under: pass --as <label> (your project name) or set $USER")
}

/// `$SHUNT_HOME/rooms/`, created 0700 like every registry dir.
fn rooms_dir() -> Result<PathBuf> {
    let d = shunt_home()?.join("rooms");
    private_dir(&d)?;
    Ok(d)
}

/// The rooms dir only if it already exists — pure read paths (`ls`,
/// `status`) must not create registry dirs just by polling.
fn existing_rooms_dir() -> Option<PathBuf> {
    let d = shunt_home().ok()?.join("rooms");
    d.is_dir().then_some(d)
}

/// Room dir names, sorted (each is a `rooms/<name>/` dir holding
/// meta.json — a stray file is not a room).
fn room_names(dir: &Path) -> Vec<String> {
    let mut names = Vec::new();
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                let n = e.file_name().to_string_lossy().into_owned();
                if !n.starts_with('.') && e.path().join("meta.json").exists() {
                    names.push(n);
                }
            }
        }
    }
    names.sort();
    names
}

/// `<room>` resolves by exact name or unique prefix (§17.2). The
/// input must be a valid name regardless — it is joined under
/// rooms/, so `..`/slashes can never escape the dir.
fn resolve(input: &str) -> Result<(String, PathBuf)> {
    valid_name(input)?;
    let dir = rooms_dir()?;
    let exact = dir.join(input);
    if exact.is_dir() && exact.join("meta.json").exists() {
        return Ok((input.to_string(), exact));
    }
    let matches: Vec<String> = room_names(&dir).into_iter().filter(|n| n.starts_with(input)).collect();
    match matches.len() {
        0 => anyhow::bail!("no room named or prefixed {input:?} — `shunt room ls --all` lists what exists, `shunt room create <name> --members p1,p2` makes one"),
        1 => {
            let n = matches.into_iter().next().unwrap();
            Ok((n.clone(), dir.join(n)))
        }
        _ => anyhow::bail!("room prefix {input:?} is ambiguous: {}", matches.join(", ")),
    }
}

fn load_meta(dir: &Path) -> Result<Meta> {
    let path = dir.join("meta.json");
    let text = fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("bad JSON in {} — fix or remove the file; `room rm` drops the whole room", path.display()))
}

/// meta.json rewrite: temp-then-rename + 0600, the registry's
/// save_with_header discipline — a concurrent `ls`/`members` must
/// never read a torn roster.
fn save_meta(dir: &Path, meta: &Meta) -> Result<()> {
    let path = dir.join("meta.json");
    let tmp = dir.join(format!(".meta.{}.tmp", std::process::id()));
    write_private(&tmp, serde_json::to_string_pretty(meta)?)?;
    fs::rename(&tmp, &path).with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Live message numbers, sorted. `msgs/.archive/` is dot-prefixed
/// and excluded — `read`/`ls` never see archived messages.
fn msg_numbers(dir: &Path) -> Vec<u32> {
    let mut ns = Vec::new();
    if let Ok(rd) = fs::read_dir(dir.join("msgs")) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if let Some(n) = name.strip_suffix(".json").and_then(|s| s.parse::<u32>().ok()).filter(|_| name.len() == "0000.json".len()) {
                ns.push(n);
            }
        }
    }
    ns.sort_unstable();
    ns
}

/// Highest NNNN anywhere in the message namespace — live msgs AND
/// .archive (the §3.2 floor spans archives too: a wrapped counter
/// minting onto an archived number collides there).
fn msg_high_water(dir: &Path) -> u32 {
    let mut hi = SEQ_INIT;
    let mut bump = |name: &std::ffi::OsString| {
        let s = name.to_string_lossy();
        if let Some(v) = s.strip_suffix(".json").and_then(|p| p.parse::<u32>().ok()).filter(|_| s.len() == "0000.json".len()) {
            hi = hi.max(v);
        }
    };
    if let Ok(rd) = fs::read_dir(dir.join("msgs")) {
        for e in rd.flatten() {
            bump(&e.file_name());
        }
    }
    if let Ok(rd) = fs::read_dir(dir.join("msgs/.archive")) {
        for e in rd.flatten() {
            bump(&e.file_name());
        }
    }
    hi
}

/// Read `.seq`, mint the next number, write it back — the §3.2
/// discipline mirrored for the message namespace. Caller holds
/// `.seq.lock`. A missing `.seq` rebuilds from the high-water mark
/// (repairable by deletion); a present-but-unparseable counter is
/// operator damage and fails with remediation, never silently
/// reseeded.
fn mint_locked(dir: &Path) -> Result<u32> {
    let seq_path = dir.join(".seq");
    let cur: u32 = match fs::read_to_string(&seq_path) {
        Ok(s) => s.trim().parse::<u32>().ok().filter(|&v| v <= SEQ_MAX).with_context(|| format!("{} is corrupt (contains {:?}); remove it to rebuild from the stream's high-water mark, or write a valid 0-{SEQ_MAX} counter", seq_path.display(), s.trim()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => SEQ_INIT,
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", seq_path.display())),
    };
    let hi = msg_high_water(dir);
    let next = if cur >= SEQ_MAX {
        // Counter wrapped the 4-digit ceiling: continue just past
        // the highest occupied number.
        hi + 1
    } else {
        (cur + 1).max(hi + 1)
    };
    if next > SEQ_MAX {
        anyhow::bail!("message sequence space exhausted: msgs/ already holds {SEQ_MAX}; `room rm` the room or prune msgs/.archive first");
    }
    write_private(&seq_path, format!("{next}\n"))?;
    Ok(next)
}

/// A label's high-water mark; absent/unreadable file = never read.
fn last_read(dir: &Path, label: &str) -> u32 {
    fs::read_to_string(dir.join("readers").join(format!("{label}.json"))).ok().and_then(|t| serde_json::from_str::<Reader>(&t).ok()).map(|r| r.last_read_n).unwrap_or(0)
}

/// `unread_msgs` for a label: count of live NNNN past the mark
/// (§17.5 — one pass over the linear stream).
fn unread_count(dir: &Path, label: &str) -> usize {
    let mark = last_read(dir, label);
    msg_numbers(dir).into_iter().filter(|&n| n > mark).count()
}

/// A read supplies its returned high-water mark; say/decide acknowledge
/// the current live maximum. Serialize the read-modify-write and eviction
/// so an older shared-label caller cannot regress a newer cursor or race
/// on the same temporary file. Message loading stays outside the lock.
fn mark(dir: &Path, label: &str, through: Option<u32>) -> Result<()> {
    let _lock = seq_lock(dir)?;
    let _ = fs::set_permissions(dir.join(".seq.lock"), fs::Permissions::from_mode(0o600));
    let max_n = through.unwrap_or_else(|| msg_numbers(dir).last().copied().unwrap_or(0)).max(last_read(dir, label));
    let rdir = dir.join("readers");
    private_dir(&rdir)?;
    let path = rdir.join(format!("{label}.json"));
    let tmp = rdir.join(format!(".tmp.{}.{label}", std::process::id()));
    write_private(&tmp, serde_json::to_string(&Reader { last_read_n: max_n })?)?;
    fs::rename(&tmp, &path).with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;

    let mut files: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    if let Ok(rd) = fs::read_dir(&rdir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || !name.ends_with(".json") {
                continue;
            }
            let mtime = e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
            files.push((mtime, e.path()));
        }
    }
    if files.len() > readers_max() {
        files.sort_by_key(|(t, p)| (*t, p.clone()));
        for (_, p) in files.iter().take(files.len() - readers_max()) {
            let _ = fs::remove_file(p);
        }
    }
    Ok(())
}

/// `room create` (§17.2). Members are project names; clients are
/// annotation context only.
pub fn create(name: &str, members: &[String], clients: &[String]) -> Result<()> {
    valid_name(name)?;
    anyhow::ensure!(!members.is_empty(), "room '{name}' needs --members p1,p2 — a room coordinates the projects sharing compute");
    let mut ms: Vec<String> = Vec::new();
    for m in members {
        let m = m.trim();
        valid_name(m).with_context(|| format!("bad member name {m:?} — members are project names"))?;
        if !ms.iter().any(|x| x == m) {
            ms.push(m.to_string());
        }
    }
    let mut cs: Vec<String> = Vec::new();
    for c in clients {
        let c = c.trim();
        valid_name(c).with_context(|| format!("bad client name {c:?}"))?;
        if !cs.iter().any(|x| x == c) {
            cs.push(c.to_string());
        }
    }
    let dir = rooms_dir()?.join(name);
    anyhow::ensure!(!dir.exists(), "room '{name}' already exists — `room members {name}` shows the roster, `room rm {name}` removes it");
    private_dir(&dir)?;
    private_dir(&dir.join("msgs"))?;
    private_dir(&dir.join("readers"))?;
    save_meta(&dir, &Meta { created_at: iso_utc(unix_now()), members: ms.clone(), clients: cs.clone(), decision: None })?;
    // Membership is declared data, not gated — but a member that is
    // not a registered project is usually a typo, so say so (the
    // same row names clients use for the same reason).
    for m in &ms {
        if registry::load_project(m).is_err() {
            eprintln!("note: member '{m}' is not a registered project — `project ls` lists what exists");
        }
    }
    for c in &cs {
        if registry::load_client(c).is_err() {
            eprintln!("note: client '{c}' is not a registered client — `client ls` lists what exists");
        }
    }
    Ok(())
}

/// `room say` (§17.2): mint under `.seq.lock`, install
/// `msgs/NNNN.json` temp-then-rename, archive the oldest past the
/// cap, then mark the caller's label (a say marks its own message
/// read — you saw what you posted). Returns the minted N and
/// whether the author is on the roster.
pub fn say(room: &str, label: &str, body: &str) -> Result<(u32, bool)> {
    anyhow::ensure!(body.len() <= MAX_BODY, "body is {} bytes; the cap is {MAX_BODY} — a coordination message carries decisions, not logs", body.len());
    let (name, dir) = resolve(room)?;
    let meta = load_meta(&dir)?;
    let member = meta.members.iter().any(|m| m == label);
    if !member {
        // Open trust: the post still lands — but name the roster so
        // a wrong-room typo is loud, not silently leaked into a
        // foreign channel (§17.3).
        eprintln!("warning: '{label}' is not a member of room '{name}' (roster: {}) — posting anyway, recorded member:false", meta.members.join(", "));
    }
    let msg = Msg { author: label.to_string(), ts: iso_utc(unix_now()), body_md: body.to_string(), member };
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
        let tmp = msgs.join(format!(".tmp.{}.{n:04}", std::process::id()));
        write_private(&tmp, serde_json::to_string_pretty(&msg)?)?;
        fs::rename(&tmp, msgs.join(format!("{n:04}.json"))).with_context(|| format!("cannot install message {n:04} in {}", msgs.display()))?;
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

/// Load one live message by number; corrupt files are skipped with
/// a warning — one bad write must not blind `read`/`ls` (the
/// feedback ledger's scan discipline).
fn load_msgs(dir: &Path) -> Vec<(u32, Msg)> {
    let mut out = Vec::new();
    for n in msg_numbers(dir) {
        let p = dir.join("msgs").join(format!("{n:04}.json"));
        match fs::read_to_string(&p).map(|t| serde_json::from_str::<Msg>(&t)) {
            Ok(Ok(m)) => out.push((n, m)),
            _ => eprintln!("warning: skipping unreadable message {}", p.display()),
        }
    }
    out
}

/// `room read` (§17.2/§17.5): returns messages past the caller's
/// mark (or the full live stream when `all`). Only successfully loaded,
/// returned messages advance the cursor; concurrent arrivals stay unread.
pub fn read(room: &str, label: &str, all: bool) -> Result<Vec<(u32, Msg)>> {
    let (_, dir) = resolve(room)?;
    let msgs = load_msgs(&dir);
    let mark0 = last_read(&dir, label);
    let returned: Vec<_> = if all { msgs } else { msgs.into_iter().filter(|(n, _)| *n > mark0).collect() };
    let through = returned.last().map(|(n, _)| *n).unwrap_or(0);
    mark(&dir, label, Some(through))?;
    Ok(returned)
}

/// `room decide` (§17.2): overwrite meta.decision — latest wins.
/// No stream message: the stream retains HOW agreement was reached;
/// the decision field is WHAT it currently is. Non-members record
/// too (open trust) with the same stderr roster note as `say`.
pub fn decide(room: &str, label: &str, note: &str) -> Result<Decision> {
    let (name, dir) = resolve(room)?;
    let mut meta = load_meta(&dir)?;
    if !meta.members.iter().any(|m| m == label) {
        eprintln!("warning: '{label}' is not a member of room '{name}' (roster: {}) — recording the decision anyway", meta.members.join(", "));
    }
    meta.decision = Some(Decision { ts: iso_utc(unix_now()), author: label.to_string(), note: note.to_string() });
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
pub fn view(name: &str, dir: &Path, meta: &Meta, label: Option<&str>) -> RoomView {
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

/// `room ls` (§17.2/§17.4): the caller's member rooms (or every
/// room under `--all`) with unread counts, plus suggested groups —
/// same-box project sets with no room yet.
pub fn list(as_flag: Option<&str>, all: bool) -> Result<(Option<String>, Vec<RoomView>, Vec<Suggested>)> {
    let label = resolve_label(as_flag)?;
    let groups = binding_groups();
    let mut views = Vec::new();
    let mut member_sets: Vec<Vec<String>> = Vec::new();
    if let Some(dir) = existing_rooms_dir() {
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
            // belongs to. `--all` widens to the directory; no
            // resolvable label at all falls back to the directory
            // too (there is nothing to scope by).
            if !all && label.as_ref().is_some_and(|l| !meta.members.contains(l)) {
                continue;
            }
            views.push(view(&name, &rd, &meta, label.as_deref()));
        }
    }
    // A group already has a room when some room's roster covers it
    // (superset: a room may span boxes — §17.4).
    let suggested = groups.into_iter().filter(|g| !member_sets.iter().any(|ms| g.projects.iter().all(|p| ms.contains(p)))).collect();
    Ok((label, views, suggested))
}

/// `room members` (§17.2): the roster view of one room.
pub fn roster(room: &str, label: Option<&str>) -> Result<RoomView> {
    let (name, dir) = resolve(room)?;
    let meta = load_meta(&dir)?;
    Ok(view(&name, &dir, &meta, label))
}

/// `room rm` (§17.4): rooms never auto-expire — the coordination
/// record is the asset — so removal is this command only.
pub fn remove(room: &str) -> Result<String> {
    let (name, dir) = resolve(room)?;
    fs::remove_dir_all(&dir).with_context(|| format!("cannot remove {}", dir.display()))?;
    Ok(name)
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
