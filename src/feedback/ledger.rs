//! Persist-first artifact ledger (design.md §14.2): every report is
//! a 0600 JSON file under `$SHUNT_HOME/feedback/` before delivery is
//! even attempted — like the spool, a delivery failure never loses
//! the report.

use super::now;
use crate::proto::task::sanitize_slug;
use crate::server::fsutil::{private_dir, write_private};
use crate::server::registry::shunt_home;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// Header-only projection for `ls` and the rate cap: report bodies
/// can be tens of KB, and metadata scans must not retain them all.
/// serde drops `body` on the way in (parsed, not retained).
#[derive(Debug, Deserialize)]
pub struct Meta {
    pub kind: String,
    pub title: String,
    pub created_at: u64,
    pub delivered_at: Option<u64>,
    pub issue_url: Option<String>,
    pub attempts: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifact {
    pub kind: String,
    /// Sanitized title without the `[feedback/<kind>]` prefix,
    /// which is re-added at delivery time, never stored.
    pub title: String,
    /// Composed issue body, already sanitized.
    pub body: String,
    pub created_at: u64,
    pub delivered_at: Option<u64>,
    pub issue_url: Option<String>,
    pub issue_number: Option<u64>,
    pub attempts: u32,
}

/// `$SHUNT_HOME/feedback/` — created 0700 like every registry dir.
pub fn dir() -> Result<PathBuf> {
    let d = shunt_home()?.join("feedback");
    private_dir(&d)?;
    Ok(d)
}

/// Write a new artifact: `<unixts>_<slug>.json`, slug = sanitized
/// title (bounded so the name stays a name); same-second collisions
/// get a `_<n>` suffix rather than an overwrite.
pub fn save_new(art: &Artifact) -> Result<PathBuf> {
    let d = dir()?;
    let slug = {
        let s = sanitize_slug(&art.title);
        let s: String = s.chars().take(48).collect();
        if s.is_empty() || s.starts_with('-') {
            "report".to_string()
        } else {
            s
        }
    };
    for n in 0..100 {
        let name = if n == 0 { format!("{}_{slug}.json", art.created_at) } else { format!("{}_{slug}_{n}.json", art.created_at) };
        let p = d.join(&name);
        if !p.exists() {
            write_private(&p, serde_json::to_string_pretty(art)?)?;
            return Ok(p);
        }
    }
    anyhow::bail!("cannot find a free feedback artifact name under {}", d.display())
}

/// Rewrite an existing artifact in place (delivery outcome update).
pub fn save_at(path: &Path, art: &Artifact) -> Result<()> {
    write_private(path, serde_json::to_string_pretty(art)?)
}

fn scan<T: serde::de::DeserializeOwned>() -> Result<Vec<(PathBuf, T)>> {
    let d = match dir() {
        Ok(d) => d,
        // No feedback dir yet = empty ledger, not an error.
        Err(_) => return Ok(Vec::new()),
    };
    let mut out = Vec::new();
    for e in fs::read_dir(&d)?.flatten() {
        let p = e.path();
        if p.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        match fs::read_to_string(&p).map(|t| serde_json::from_str::<T>(&t)) {
            Ok(Ok(a)) => out.push((p, a)),
            _ => eprintln!("warning: skipping unreadable feedback artifact {}", p.display()),
        }
    }
    Ok(out)
}

/// Metadata only — `ls`/`delivered_count` never retain report bodies.
pub fn list_meta() -> Result<Vec<(PathBuf, Meta)>> {
    let mut out = scan::<Meta>()?;
    out.sort_by_key(|(_, a)| a.created_at);
    Ok(out)
}

/// Full artifacts oldest-first — the paths that need bodies
/// (`find_dup` compares them, `retry` re-delivers them). A corrupt
/// file is skipped with a stderr warning — one bad write must not
/// blind `ls`/`retry` to the rest.
pub fn list() -> Result<Vec<(PathBuf, Artifact)>> {
    let mut out = scan::<Artifact>()?;
    out.sort_by_key(|(_, a)| a.created_at);
    Ok(out)
}

/// A delivered artifact with identical (kind, title, body) in the
/// last `within` seconds — the dedup hit `send` short-circuits to.
pub fn find_dup(art: &Artifact, within: u64) -> Result<Option<Artifact>> {
    let cutoff = now().saturating_sub(within);
    Ok(list()?.into_iter().map(|(_, a)| a).find(|a| a.delivered_at.is_some_and(|t| t >= cutoff) && a.kind == art.kind && a.title == art.title && a.body == art.body))
}

/// Delivered artifacts in the last `within` seconds — the rate cap.
/// Only real deliveries count (`attempts > 0`): a dedup-hit copy is
/// marked delivered but consumed no API call.
pub fn delivered_count(within: u64) -> Result<u64> {
    let cutoff = now().saturating_sub(within);
    Ok(list_meta()?.iter().filter(|(_, a)| a.delivered_at.is_some_and(|t| t >= cutoff) && a.attempts > 0).count() as u64)
}
