//! Coordination rooms (design.md §17): a long-lived named group
//! chat whose members are the PROJECTS sharing compute — the side
//! channel the queue deliberately withholds. Everything is
//! server-local under `$SHUNT_HOME/rooms/<name>/`: no wire op, no
//! box-side state, no interaction with scheduling or the
//! idle-shutdown gate. `meta.json` holds the roster + the current
//! decision; `msgs/NNNN.json` is one linear stream minted under
//! `.seq.lock`; `readers/<label>.json` is each identity's
//! high-water mark.

use crate::agent::queue::seq_lock;
use crate::proto::task::sanitize_slug;
use crate::proto::time::rfc3339_z_now;
use crate::server::fsutil::{atomic_write, private_dir, tmp_sibling, write_private};
use crate::server::registry::names::{self, Resolution};
use crate::server::registry::{self, shunt_home, valid_name};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
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

/// Compaction floor: a digest over a short stream saves nothing —
/// plan refuses below this many digestible live messages and points
/// at the manual `--through` escape. `SHUNT_ROOM_COMPACT_MIN`
/// overrides (tests).
fn compact_min() -> usize {
    std::env::var("SHUNT_ROOM_COMPACT_MIN").ok().and_then(|s| s.parse().ok()).unwrap_or(100)
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
    /// Compaction ratchet: the highest N covered by a committed
    /// digest. Lives in meta, not derivable from the stream — the
    /// 500-cap can archive the newest summary itself, taking the
    /// record of what it covered with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compacted_through: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    pub ts: String,
    pub author: String,
    pub note: String,
}

/// One stream entry: `msgs/NNNN.json`. `member: false` marks a
/// post from outside the roster — allowed (open trust), but loud.
/// `summary_through` presence alone marks an agent-written
/// compaction digest; its value is the highest covered N (no
/// separate `kind` field — old binaries just ignore it).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Msg {
    pub author: String,
    pub ts: String,
    pub body_md: String,
    pub member: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_through: Option<u32>,
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

/// `admin` (case-insensitive in every comparison) is the human
/// operator's reserved identity (§17.3). No `--as` means the
/// operator — an agent forgetting `--as` posts as a person, not
/// under a stray $USER. `admin` is exempt from the non-member
/// warning (a human reading a foreign room is overseeing, not
/// leaking) yet still records `member: false` — it is honestly not
/// on the roster. It can never BECOME a roster member or a
/// registered project: both are rejected at intake, so no lookup
/// can turn the operator into a project.
pub const ADMIN_LABEL: &str = "admin";

fn is_admin(label: &str) -> bool {
    label.eq_ignore_ascii_case(ADMIN_LABEL)
}

/// Roster membership is case-insensitive (§17.3): canonicalization
/// already aligns registered project names, and the insensitive
/// compare extends the same courtesy to rosters holding
/// non-registered member names.
fn on_roster(meta: &Meta, label: &str) -> bool {
    meta.members.iter().any(|m| m.eq_ignore_ascii_case(label))
}

/// Open trust: a non-member op still lands — but name the roster so
/// a wrong-room typo is loud, not silently leaked into a foreign
/// channel (§17.3). `admin` is exempt: the operator is the one
/// reader the warning would be noise for.
fn warn_non_member(name: &str, meta: &Meta, label: &str, action: &str) {
    if !is_admin(label) && !on_roster(meta, label) {
        eprintln!("warning: '{label}' is not a member of room '{name}' (roster: {}) — {action}", meta.members.join(", "));
    }
}

/// The caller's `--as` label: author of messages AND key of the
/// readers/ cursor — you read under the name you speak under
/// (§17.3). Resolution: explicit `--as` > `admin`. The value is
/// sanitized like a §3.3 slug, then must also be a valid registry
/// name (it becomes `readers/<label>.json` — a `..` must not
/// escape), then run through the registry name middleware (§4.1):
/// `admin` in any case folds to `admin`, and a unique case-fold hit
/// against registered projects substitutes the canonical spelling
/// silently — membership checks, `author`, and the cursor all agree
/// on who this is. Two registered projects differing only by case
/// make the label ambiguous — a loud error, not a coin flip; a
/// label matching nothing is free-form and passes through.
pub(crate) fn resolve_label(as_flag: Option<&str>) -> Result<String> {
    let raw = as_flag.unwrap_or(ADMIN_LABEL);
    let label = sanitize_slug(raw);
    valid_name(&label).with_context(|| format!("bad --as label {raw:?} (sanitized to {label:?}): use letters, digits, '_', '.', '-' — your project name is the convention"))?;
    if is_admin(&label) {
        return Ok(ADMIN_LABEL.to_string());
    }
    match names::resolve_project(&label)? {
        Resolution::Exact(p) | Resolution::Canonical { canonical: p, .. } => Ok(p),
        Resolution::Ambiguous(projects) => anyhow::bail!("--as label {label:?} is ambiguous: registered projects {} differ only by case — rename one", projects.join(", ")),
        // Free-form labels stay legal (§17.3): a non-member warning
        // still fires downstream where the roster is known.
        Resolution::Unknown { .. } => Ok(label),
    }
}

/// A marking op's label — same resolution; the name keeps call
/// sites honest about why an identity is required (§17.3).
pub(crate) fn required_label(as_flag: Option<&str>) -> Result<String> {
    resolve_label(as_flag)
}

/// Cold-storage namespace (§17): `rooms/.archive/` holds whole
/// room dirs — a room dir is fully self-contained (meta.json,
/// msgs/ incl. msgs/.archive, readers/, .seq), so one rename moves
/// the entire record between live and archived with no format
/// work. Dot-prefixed, so `room_names` keeps the live candidate
/// set disjoint from the archive for free.
const ARCHIVE_DIR: &str = ".archive";

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

/// Room dir names, sorted — the candidate set lives in
/// `registry::names` so listing and resolving share one predicate.
fn room_names(dir: &Path) -> Vec<String> {
    names::room_names(dir)
}

/// The shared live-miss error — `resolve` and `remove` each run
/// their own archived-name check first, then build this identically
/// so the wording never drifts between them.
fn unknown_room_err(input: &str, suggestions: &[String]) -> anyhow::Error {
    anyhow::anyhow!("no room named or prefixed {input:?}{} — `shunt room ls --all` lists what exists, `shunt room create <name> --members p1,p2` makes one", names::did_you_mean(suggestions))
}

/// `<room>` resolves through the name middleware (§4.1): exact,
/// then unique case-fold or unique prefix (§17.2's accepted
/// resolutions, both reported as `Canonical` and taken silently),
/// ambiguous prefix a loud bail. The input must be a valid name
/// regardless — it is joined under rooms/, so `..`/slashes can
/// never escape the dir.
fn resolve(input: &str) -> Result<(String, PathBuf)> {
    valid_name(input)?;
    let dir = rooms_dir()?;
    match names::resolve_room(input)? {
        Resolution::Exact(name) | Resolution::Canonical { canonical: name, .. } => Ok((name.clone(), dir.join(&name))),
        Resolution::Unknown { input, suggestions } => {
            // An exact archived namesake is not "unknown" — the
            // error names the restore path instead. Exact-only:
            // cold storage names are typed in full, never
            // prefix-matched (the prefix ladder stays live-only).
            if dir.join(ARCHIVE_DIR).join(&input).is_dir() {
                anyhow::bail!("room '{input}' is archived — `shunt room unarchive {input}` restores it");
            }
            Err(unknown_room_err(&input, &suggestions))
        }
        Resolution::Ambiguous(cands) => anyhow::bail!("room prefix {input:?} is ambiguous: {}", cands.join(", ")),
    }
}

fn load_meta(dir: &Path) -> Result<Meta> {
    let path = dir.join("meta.json");
    let text = fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("bad JSON in {} — fix or remove the file; `room rm` drops the whole room", path.display()))
}

/// meta.json rewrite via fsutil::atomic_write — a concurrent
/// `ls`/`members` must never read a torn roster.
fn save_meta(dir: &Path, meta: &Meta) -> Result<()> {
    atomic_write(&dir.join("meta.json"), serde_json::to_string_pretty(meta)?)
}

/// Sorted NNNN files under one message dir (live `msgs/` or
/// `msgs/.archive/` — same numbering namespace).
fn msg_numbers_in(msgs_dir: &Path) -> Vec<u32> {
    let mut ns = Vec::new();
    if let Ok(rd) = fs::read_dir(msgs_dir) {
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

/// Live message numbers, sorted. `msgs/.archive/` is dot-prefixed
/// and excluded — `read`/`ls` never see archived messages.
fn msg_numbers(dir: &Path) -> Vec<u32> {
    msg_numbers_in(&dir.join("msgs"))
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
    atomic_write(&rdir.join(format!("{label}.json")), serde_json::to_string(&Reader { last_read_n: max_n })?)?;

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

fn initialize_room(dir: &Path, initialize: impl FnOnce(&Path) -> Result<()>) -> Result<()> {
    // mkdir is the ownership boundary: even an empty pre-existing directory
    // belongs to somebody else and must not be initialized or chmodded.
    fs::DirBuilder::new().mode(0o700).create(dir).with_context(|| format!("cannot create room {} exclusively — if it already exists, use `room members` or `room rm`", dir.display()))?;
    let owned = fs::symlink_metadata(dir)?;
    let result = fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(anyhow::Error::from).and_then(|()| initialize(dir));
    if result.is_err() {
        // Never recursively delete: a replacement or unexpected content must
        // survive a failed initializer. Inode identity also protects an empty
        // replacement, which remove_dir alone would otherwise accept.
        if fs::symlink_metadata(dir).is_ok_and(|m| m.dev() == owned.dev() && m.ino() == owned.ino()) {
            let cleanup = || -> std::io::Result<()> {
                // save_meta's staging sibling, if it stranded mid-write.
                let tmp = tmp_sibling(&dir.join("meta.json"));
                if tmp.exists() {
                    fs::remove_file(tmp)?;
                }
                for child in ["msgs", "readers"] {
                    match fs::remove_dir(dir.join(child)) {
                        Ok(()) => (),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                        Err(e) => return Err(e),
                    }
                }
                fs::remove_dir(dir)
            };
            if let Err(error) = cleanup() {
                return result.with_context(|| format!("partial room {} retained: cleanup failed: {error}; inspect before retrying", dir.display()));
            }
        }
    }
    result
}

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

/// `room say` (§17.2): mint under `.seq.lock`, install
/// `msgs/NNNN.json` temp-then-rename, archive the oldest past the
/// cap, then mark the caller's label (a say marks its own message
/// read — you saw what you posted). Returns the minted N and
/// whether the author is on the roster.
pub fn say(room: &str, label: &str, body: &str) -> Result<(u32, bool)> {
    anyhow::ensure!(body.len() <= MAX_BODY, "body is {} bytes; the cap is {MAX_BODY} — a coordination message carries decisions, not logs", body.len());
    let (name, dir) = resolve(room)?;
    let meta = load_meta(&dir)?;
    let member = on_roster(&meta, label);
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

fn load_msgs(dir: &Path) -> Vec<(u32, Msg)> {
    load_msgs_from(&dir.join("msgs"))
}

/// The archive loader — range reads and the digest fallback reach
/// across the cap boundary.
fn load_archived_msgs(dir: &Path) -> Vec<(u32, Msg)> {
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
fn split_index(bytes: &[usize], ratio: f64) -> usize {
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

/// The `room ls` report (§17.2/§17.4): the resolved reader label,
/// that label's member rooms (or every room under `--all`) with
/// unread counts, suggested same-box project groups, and the
/// archived room names (cold storage: names only, no cursor work).
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

#[cfg(test)]
mod compact_tests {
    use super::*;

    #[test]
    fn split_index_weights_bytes_not_count() {
        // A fat first message pulls the cut early: count-80% of 4
        // would land on the last index, byte-80% lands on index 1.
        assert_eq!(split_index(&[50, 10, 10, 10], 0.8), 1);
        // An exact byte hit splits there.
        assert_eq!(split_index(&[10, 10, 10, 10, 10], 0.8), 3);
        // Equal distance to the target takes the earlier index.
        assert_eq!(split_index(&[10, 10], 0.75), 0);
        // A single message can only split at itself.
        assert_eq!(split_index(&[100], 0.5), 0);
    }
}

#[cfg(test)]
mod creation_tests {
    use super::*;

    #[test]
    fn initialization_requires_exclusive_directory_ownership() {
        let dir = std::env::temp_dir().join(format!("shunt-room-exclusive-{}", std::process::id()));
        let _ = fs::remove_dir(&dir);
        fs::create_dir(&dir).unwrap();
        let mut initialized = false;
        let result = initialize_room(&dir, |_| {
            initialized = true;
            Ok(())
        });
        fs::remove_dir(&dir).unwrap();
        assert!(!initialized, "an existing directory must never be initialized");
        assert!(result.is_err());
    }
    #[test]
    fn failed_initialization_cleans_only_its_own_directory() {
        let dir = std::env::temp_dir().join(format!("shunt-room-failure-{}", std::process::id()));
        let moved = dir.with_extension("original");
        let result = initialize_room(&dir, |dir| {
            fs::create_dir(dir.join("msgs"))?;
            fs::create_dir(dir.join("readers"))?;
            fs::write(tmp_sibling(&dir.join("meta.json")), b"partial")?;
            anyhow::bail!("injected initialization failure")
        });
        assert!(result.is_err());
        assert!(!dir.exists(), "failed initialization must permit retry");
        let result = initialize_room(&dir, |dir| {
            fs::rename(dir, &moved)?;
            fs::create_dir(dir)?;
            anyhow::bail!("injected replacement")
        });
        assert!(result.is_err());
        assert!(dir.is_dir(), "another owner's empty directory must survive");
        fs::remove_dir(dir).unwrap();
        fs::remove_dir(moved).unwrap();
    }
}
