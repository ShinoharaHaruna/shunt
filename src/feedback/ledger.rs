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
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

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
    let contents = serde_json::to_vec_pretty(art)?;
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    let (temp, mut file) = loop {
        let p = d.join(format!(".pending-{}-{}", std::process::id(), NEXT_TEMP.fetch_add(1, Ordering::Relaxed)));
        match fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&p) {
            Ok(f) => break (p, f),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    };
    let result = (|| {
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(&contents)?;
        file.sync_all()?;
        // Scans ignore the staging name. A hard link publishes complete JSON
        // atomically and, unlike rename, cannot replace another writer's report.
        for n in 0..100 {
            let name = if n == 0 { format!("{}_{slug}.json", art.created_at) } else { format!("{}_{slug}_{n}.json", art.created_at) };
            let p = d.join(&name);
            match fs::hard_link(&temp, &p) {
                Ok(()) => return Ok(p),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        }
        anyhow::bail!("cannot find a free feedback artifact name under {} (100 candidates occupied); wait a second and re-run `shunt feedback`", d.display())
    })();
    if let Err(e) = fs::remove_file(&temp) {
        eprintln!("warning: cannot remove feedback staging file {} ({e})", temp.display());
    }
    result
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    #[test]
    fn save_new_exclusively_installs_complete_private_json() {
        let _guard = crate::server::registry::tests::HOME_LOCK.lock().unwrap();
        let home = std::env::temp_dir().join(format!("shunt-ledger-exclusive-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        std::env::set_var("SHUNT_HOME", &home);
        let d = dir().unwrap();
        let occupied = d.join("123_same-title.json");
        let target = home.join("must-not-be-created");
        // A dangling symlink is an occupied name even though exists() is false.
        // This deterministically exercises the exclusive no-clobber contract.
        symlink(&target, &occupied).unwrap();
        let mut art = Artifact { kind: "bug".into(), title: "same title".into(), body: "body 1".into(), created_at: 123, delivered_at: None, issue_url: None, issue_number: None, attempts: 0 };
        let first = save_new(&art).unwrap();
        assert_ne!(first, occupied, "an occupied name must never be followed or overwritten");
        assert!(!target.exists());
        assert!(fs::symlink_metadata(&occupied).unwrap().file_type().is_symlink());
        fs::remove_file(&occupied).unwrap();
        for i in 0..99 {
            art.body = format!("distinct body {i}");
            let p = save_new(&art).unwrap();
            assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
            let saved: Artifact = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
            assert_eq!(saved.body, art.body);
        }
        assert_eq!(list().unwrap().len(), 100);
        assert_eq!(fs::read_dir(&d).unwrap().count(), 100, "temporary files must be cleaned up");
        assert_eq!(serde_json::from_slice::<Artifact>(&fs::read(first).unwrap()).unwrap().body, "body 1");
        let err = save_new(&art).unwrap_err().to_string();
        assert!(err.contains("100 candidates occupied"), "{err}");
        assert!(err.contains("re-run `shunt feedback`"), "{err}");
        assert_eq!(list().unwrap().len(), 100);
        assert_eq!(fs::read_dir(&d).unwrap().count(), 100, "exhaustion must also clean up staging");
        std::env::remove_var("SHUNT_HOME");
        fs::remove_dir_all(home).unwrap();
    }
}
