//! Name-resolution middleware (design.md §4.1 "Name resolution"):
//! one pure matcher plus thin per-domain wrappers. Every typed
//! name — `-c`, `--queue`, `--project`, `--as`, a room, a
//! `--members` entry — resolves through the same ladder:
//!
//!   exact as typed → unique case-fold (canonical spelling
//!   substituted) → suggestions only (prefix + fuzzy).
//!
//! Suggestions are error-message material only: they never rewrite
//! identity and are never auto-accepted — picking the wrong box is
//! worse than a loud miss. The one historical exception is room
//! names, which accepted a unique prefix before this module
//! existed (§17.2); `resolve_room` keeps that acceptance, reported
//! as `Canonical` so callers can still tell it from an as-typed hit.

use super::{list_clients, list_names, queue_names, shunt_home, ClientEntry};
use anyhow::Result;
use std::path::Path;

/// The outcome of resolving one typed name against a candidate set.
/// Callers pick their loudness from it: substitute-and-announce,
/// substitute silently, or bail with `suggestions` in the error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// Hit exactly as typed.
    Exact(String),
    /// Unique case-fold hit — substitute `canonical` for `typed`
    /// (`typed` is carried so callers can announce). `resolve_room`
    /// also reports its unique-prefix acceptance here.
    Canonical { canonical: String, typed: String },
    /// No acceptable hit. `suggestions` (prefix then fuzzy hits)
    /// is error-message material only — never auto-accepted.
    Unknown { input: String, suggestions: Vec<String> },
    /// Several candidates differ only by case — resolving would be
    /// a coin flip, so callers bail.
    Ambiguous(Vec<String>),
}

/// Core matcher: exact → unique case-fold → suggestion list. Pure;
/// every loudness decision lives at the call sites via `Resolution`.
pub fn resolve(input: &str, candidates: &[String]) -> Resolution {
    if candidates.iter().any(|c| c == input) {
        return Resolution::Exact(input.to_string());
    }
    let folded: Vec<String> = candidates.iter().filter(|c| c.eq_ignore_ascii_case(input)).cloned().collect();
    match folded.len() {
        1 => return Resolution::Canonical { canonical: folded.into_iter().next().unwrap(), typed: input.to_string() },
        n if n > 1 => return Resolution::Ambiguous(folded),
        _ => {}
    }
    Resolution::Unknown { input: input.to_string(), suggestions: suggestions(input, candidates) }
}

/// Prefix hits first (sorted), then fuzzy hits by distance then
/// name. Case-insensitive throughout: case already had its own
/// ladder rung, so it should not burn the edit budget here either.
fn suggestions(input: &str, candidates: &[String]) -> Vec<String> {
    let low = input.to_lowercase();
    let mut out: Vec<String> = candidates.iter().filter(|c| c.to_lowercase().starts_with(&low)).cloned().collect();
    out.sort();
    let budget = fuzzy_budget(&low);
    let mut fuzzy: Vec<(usize, String)> = candidates
        .iter()
        .filter(|c| !out.iter().any(|o| o == *c))
        .filter_map(|c| {
            let d = edit_distance(&low, &c.to_lowercase());
            (d <= budget).then(|| (d, c.clone()))
        })
        .collect();
    fuzzy.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    out.extend(fuzzy.into_iter().map(|(_, c)| c));
    out
}

/// Levenshtein budget scaled to input length: names up to 4 chars
/// earn a single edit — two edits on a short name is noise, not
/// help — longer names earn two.
fn fuzzy_budget(input: &str) -> usize {
    if input.chars().count() <= 4 {
        1
    } else {
        2
    }
}

/// Small std-only Levenshtein over chars. Suggestion-path only —
/// it never feeds an acceptance decision.
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.chars().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + usize::from(ca != *cb));
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// `Unknown`-error suffix: " — did you mean: a, b" or empty.
pub fn did_you_mean(suggestions: &[String]) -> String {
    if suggestions.is_empty() {
        String::new()
    } else {
        format!(" — did you mean: {}", suggestions.join(", "))
    }
}

/// The standard loudness policy for typed references: `Exact`
/// passes, `Canonical` substitutes and announces on stderr (a
/// name rewritten behind the user's back deserves a spoken word),
/// `Unknown`/`Ambiguous` bail. `kind` names the domain in the
/// messages ("client", "queue", "project"). Callers that need
/// silence or different wording match on `Resolution` themselves.
pub fn into_resolved(res: Resolution, kind: &str) -> Result<String> {
    match res {
        Resolution::Exact(name) => Ok(name),
        Resolution::Canonical { canonical, typed } => {
            eprintln!("note: resolved '{typed}' → {kind} '{canonical}'");
            Ok(canonical)
        }
        Resolution::Unknown { input, suggestions } => anyhow::bail!("unknown {kind} {input:?}{}", did_you_mean(&suggestions)),
        Resolution::Ambiguous(cands) => anyhow::bail!("{kind} name is ambiguous — candidates differ only by case: {}", cands.join(", ")),
    }
}

/// Registered project names (`projects/*.toml` stems).
pub fn resolve_project(input: &str) -> Result<Resolution> {
    Ok(resolve(input, &list_names("projects")?))
}

/// Usable clients (`list_clients`: skips disabled rows, includes
/// the pre-registered `localhost`).
pub fn resolve_client(input: &str) -> Result<Resolution> {
    Ok(resolve(input, &list_clients()?))
}

/// Queue names registered on `client` (its `[queues]` map, or the
/// literal `default` on legacy rows).
pub fn resolve_queue(input: &str, client: &ClientEntry) -> Result<Resolution> {
    Ok(resolve(input, &queue_names(client)))
}

/// Room names (`rooms/<name>/` dirs holding meta.json). Unlike the
/// other wrappers, a unique prefix is still an accepted resolution —
/// §17.2's original rule — reported as `Canonical`; a prefix hitting
/// several rooms is `Ambiguous`, the same loud bail as before.
pub fn resolve_room(input: &str) -> Result<Resolution> {
    let candidates = room_names(&shunt_home()?.join("rooms"));
    match resolve(input, &candidates) {
        Resolution::Unknown { input, suggestions } => {
            let low = input.to_lowercase();
            let prefix: Vec<String> = candidates.iter().filter(|c| c.to_lowercase().starts_with(&low)).cloned().collect();
            match prefix.len() {
                1 => Ok(Resolution::Canonical { canonical: prefix.into_iter().next().unwrap(), typed: input }),
                n if n > 1 => Ok(Resolution::Ambiguous(prefix)),
                _ => Ok(Resolution::Unknown { input, suggestions }),
            }
        }
        r => Ok(r),
    }
}

/// `rooms/<name>/` dirs holding a meta.json, sorted — the
/// room-name candidate set (shared with `server::room` so listing
/// and resolving never disagree; a stray file is not a room).
pub(crate) fn room_names(dir: &Path) -> Vec<String> {
    let mut names = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cands(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    /// The resolution ladder: exact beats everything, unique
    /// case-fold substitutes the canonical spelling carrying the
    /// typed form, case-variant duplicates are a loud ambiguity.
    #[test]
    fn exact_canonical_ambiguous() {
        assert_eq!(resolve("rw-x", &cands(&["rw-x", "gpu-1"])), Resolution::Exact("rw-x".into()));
        assert_eq!(resolve("RW-X", &cands(&["rw-x", "gpu-1"])), Resolution::Canonical { canonical: "rw-x".into(), typed: "RW-X".into() });
        // An exact hit always wins over a case-variant twin.
        assert_eq!(resolve("rwx", &cands(&["RWX", "rwx"])), Resolution::Exact("rwx".into()));
        assert_eq!(resolve("rwx", &cands(&["RWX", "RwX"])), Resolution::Ambiguous(cands(&["RWX", "RwX"])));
    }

    /// Prefix and fuzzy hits populate `suggestions` only — none of
    /// them is ever accepted.
    #[test]
    fn unknown_carries_suggestions() {
        // Prefix hit (case-insensitive), leading the list.
        match resolve("ribo", &cands(&["RiboWorld", "other"])) {
            Resolution::Unknown { input, suggestions } => {
                assert_eq!(input, "ribo");
                assert_eq!(suggestions, vec!["RiboWorld"]);
            }
            r => panic!("expected Unknown, got {r:?}"),
        }
        // Pure fuzzy hit, no prefix.
        match resolve("gpuu-1", &cands(&["gpu-1", "zzz"])) {
            Resolution::Unknown { suggestions, .. } => assert_eq!(suggestions, vec!["gpu-1"]),
            r => panic!("expected Unknown, got {r:?}"),
        }
        // Nothing close: empty suggestions, no panic.
        match resolve("zzz", &cands(&["gpu-1"])) {
            Resolution::Unknown { suggestions, .. } => assert!(suggestions.is_empty()),
            r => panic!("expected Unknown, got {r:?}"),
        }
    }

    /// The fuzzy budget scales with input length: ≤4 chars earn one
    /// edit (two would be noise), longer names earn two — and three
    /// edits is always out of reach.
    #[test]
    fn fuzzy_threshold_scales_with_name_length() {
        // "g1" (2 chars, budget 1): "g2"/"g9" at distance 1 land;
        // "h9" is distance 2 (both chars differ) and "gpu-1" is far
        // out — neither suggests on a short name.
        match resolve("g1", &cands(&["g2", "g9", "h9", "gpu-1"])) {
            Resolution::Unknown { suggestions, .. } => assert_eq!(suggestions, vec!["g2", "g9"]),
            r => panic!("expected Unknown, got {r:?}"),
        }
        // Long input: distance 2 still suggests.
        match resolve("ribowrld", &cands(&["RiboWorld"])) {
            Resolution::Unknown { suggestions, .. } => assert_eq!(suggestions, vec!["RiboWorld"]),
            r => panic!("expected Unknown, got {r:?}"),
        }
        // ...but distance 3 is out of budget even for a long name.
        match resolve("riboXXXd", &cands(&["RiboWorld"])) {
            Resolution::Unknown { suggestions, .. } => assert!(suggestions.is_empty()),
            r => panic!("expected Unknown, got {r:?}"),
        }
        // Short input stays tight: distance 2 never suggests.
        match resolve("ab", &cands(&["axyz"])) {
            Resolution::Unknown { suggestions, .. } => assert!(suggestions.is_empty()),
            r => panic!("expected Unknown, got {r:?}"),
        }
    }

    /// Ordering: prefix hits lead in candidate order; fuzzy follows
    /// sorted by distance then name; no duplicates.
    #[test]
    fn suggestions_order_and_dedup() {
        match resolve("box", &cands(&["box-b", "box-a", "boz"])) {
            Resolution::Unknown { suggestions, .. } => assert_eq!(suggestions, vec!["box-a", "box-b", "boz"]),
            r => panic!("expected Unknown, got {r:?}"),
        }
    }
}
