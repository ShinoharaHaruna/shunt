//! Task file model. A task on disk is a `NNNN_<slug>/` entry dir
//! holding `task.sh` (design.md §3.1); the metadata here is what the
//! generator (`shunt new`/`mktask`) and the spool carry, not a file
//! format of its own.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Metadata for one task, as carried in the server spool entry and
/// used to render the task body (design.md §3.13, §4.4).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskMeta {
    /// Server-side task id; emitted as `# shunt:id=` in the body so
    /// the box-side allocator can dedup a re-pushed delivery
    /// (design.md §3.3, §4.4). `None` for locally-generated tasks
    /// (`new`/`mktask`) — dedup simply does not apply to them.
    #[serde(default)]
    pub id: Option<String>,
    /// Human label; sanitized to `[A-Za-z0-9_.-]` before use.
    pub slug: String,
    /// Guard path: if it exists at run time, the task exits 0 (the
    /// idempotency contract behind at-least-once).
    pub guard: Option<String>,
    /// cwd the runner places the task in (from the project binding).
    pub workdir: Option<String>,
    /// Env overlay injected by the runner (PATH_PREPEND, TASK_THREADS).
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Command and args, executed via `exec bash <taskfile>` equivalent.
    #[serde(default)]
    pub cmd: Vec<String>,
    /// Wall-clock budget in seconds; `None`/0 = unlimited. Serde
    /// default keeps pre-timeout spool entries parseable, and the
    /// rendered `# shunt:timeout_s=` head line is a plain comment to
    /// old runners (design.md §3.13) — backward compatible both ways.
    #[serde(default)]
    pub timeout_s: Option<u64>,
    /// Reschedule budget: a task exiting rc 75 (EX_TEMPFAIL — its
    /// own "resource was busy" signal) is requeued instead of
    /// finalized, up to N extra runs (design.md §3.9). `None`/0 =
    /// never reschedule; every other nonzero rc finalizes normally.
    #[serde(default)]
    pub retry: Option<u32>,
    /// Seconds a rescheduled entry waits in pending/ before it can
    /// be claimed again (`retry_after` sidecar, design.md §3.7).
    /// `None` = daemon default (60 s).
    #[serde(default)]
    pub retry_delay_s: Option<u64>,
    /// Declared resource estimates — ANNOTATION ONLY, nothing is
    /// ever gated on them (design.md §3.16): the daemon measures
    /// actual peaks into the entry's `usage` sidecar and the agent
    /// refines its next estimate against the observed diff.
    #[serde(default)]
    pub gpu_mem_mb: Option<u32>,
    #[serde(default)]
    pub ram_mb: Option<u32>,
    #[serde(default)]
    pub cpu_cores: Option<u32>,
}

/// CLI knobs that map 1:1 onto `TaskMeta` (`submit`/`new` flags).
/// Bundled so `submit` doesn't grow a parameter per directive.
#[derive(Debug, Default, Clone)]
pub struct TaskFlags {
    pub timeout_s: Option<u64>,
    pub retry: Option<u32>,
    pub retry_delay_s: Option<u64>,
    pub gpu_mem_mb: Option<u32>,
    pub ram_mb: Option<u32>,
    pub cpu_cores: Option<u32>,
    pub env: BTreeMap<String, String>,
}

/// Parse repeated `--env KEY=value` flags into an overlay map. A
/// bare `--env X` is an error, not a silent empty value — the caller
/// meant a key and mistyped it.
pub fn parse_env_pairs(pairs: &[String]) -> anyhow::Result<BTreeMap<String, String>> {
    let mut env = BTreeMap::new();
    for p in pairs {
        let (k, v) = p.split_once('=').with_context(|| format!("--env expects KEY=value, got {p:?}"))?;
        anyhow::ensure!(!k.is_empty(), "--env expects a non-empty key, got {p:?}");
        env.insert(k.to_string(), v.to_string());
    }
    Ok(env)
}

/// A staged server-side spool entry: metadata plus the rendered body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpoolEntry {
    /// Server-side task id; embedded in the pushed slug as
    /// `<slug>-<shortid>` so retries are recognizable.
    pub id: String,
    pub project: String,
    pub client: String,
    pub meta: TaskMeta,
    pub submitted_at: String,
}

/// Sanitize a slug to `[A-Za-z0-9_.-]`; anything else becomes `-`.
/// An empty or dash-leading result is an error upstream
/// (design.md §3.3); check with [`slug_problem`].
pub fn sanitize_slug(raw: &str) -> String {
    raw.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') { c } else { '-' }).collect()
}

/// Why a sanitized slug is unusable (`None` = fine). A leading `-`
/// is rejected: the slug rides in remote argv (`shunt enqueue -
/// <slug>`) where a dash parses as a flag, and a pushed entry that
/// the remote rejects stays in spool and retries forever.
pub fn slug_problem(slug: &str) -> Option<&'static str> {
    if slug.is_empty() {
        Some("empty after sanitization")
    } else if slug.starts_with('-') {
        Some("starts with '-': would parse as a flag in remote argv")
    } else {
        None
    }
}

/// The delivery-dedup key a body carries in its comment head
/// (`# shunt:id=<server task id>`, design.md §3.3/§3.13): `submit`
/// stamps it, and the box-side allocator returns the existing queue
/// entry for a re-pushed body instead of minting a second number.
/// Same head-scan rules as the runner's directive parser: leading
/// whitespace still counts as a comment, scanning stops at the
/// first non-comment, non-blank line. `None` for hand-written
/// bodies without the directive — dedup does not apply to them.
pub fn dedup_key(body: &str) -> Option<String> {
    for line in body.lines() {
        let line = line.trim_start();
        if let Some(id) = line.strip_prefix("# shunt:id=") {
            // A token, not a literal like workdir: trailing
            // whitespace can never match the stored key.
            let id = id.trim();
            if !id.is_empty() {
                return Some(id.to_string());
            }
        } else if !line.starts_with('#') && !line.trim().is_empty() {
            break;
        }
    }
    None
}

/// Reject meta fields that would inject extra lines into the
/// rendered task body: `workdir`/`guard`/env keys and values ride in
/// `# shunt:` comment lines, so a `\n`/`\r` inside any of them
/// (TOML escapes make these real) splices arbitrary lines — even
/// code lines — into the file. Must run at every render entry point
/// before `render_task`/`merge_file_directives`; `cmd` is exempt
/// (shell-quoted, so embedded newlines stay inside quotes).
pub fn validate_meta(meta: &TaskMeta) -> anyhow::Result<()> {
    let fields = std::iter::once(("slug", meta.slug.as_str())).chain(meta.id.iter().map(|i| ("id", i.as_str()))).chain(meta.guard.iter().map(|g| ("guard", g.as_str()))).chain(meta.workdir.iter().map(|w| ("workdir", w.as_str()))).chain(meta.env.iter().flat_map(|(k, v)| [("env key", k.as_str()), ("env value", v.as_str())]));
    for (what, s) in fields {
        anyhow::ensure!(!s.contains(['\n', '\r']), "task meta {what} contains a newline — would inject lines into the task body");
    }
    // An env key containing `=` cannot round-trip: rendered as
    // `# shunt-env:K=X=v`, the runner's split_once('=') reads the
    // key as `K` and `X=v` as the value — a silent rename. Reject
    // it here rather than let the directive lie (design.md §3.12).
    for k in meta.env.keys() {
        anyhow::ensure!(!k.contains('='), "task meta env key {k:?} contains '=' — cannot round-trip through # shunt-env:KEY=value");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::sanitize_slug;

    #[test]
    fn slug_keeps_safe_chars() {
        assert_eq!(sanitize_slug("train_v2.1-final"), "train_v2.1-final");
    }

    #[test]
    fn slug_replaces_unsafe_chars() {
        assert_eq!(sanitize_slug("a b/c:d"), "a-b-c-d");
        assert_eq!(sanitize_slug("实验A"), "--A");
    }

    #[test]
    fn slug_empty_stays_empty() {
        assert_eq!(sanitize_slug(""), "");
    }

    #[test]
    fn slug_problem_flags_dash_and_empty() {
        use super::slug_problem;
        assert!(slug_problem("-rf").is_some());
        assert!(slug_problem("--help").is_some());
        assert!(slug_problem("").is_some());
        assert!(slug_problem("train-v2").is_none());
        // non-ASCII slugs sanitize to leading dashes -> rejected too
        assert!(slug_problem(&sanitize_slug("实验A")).is_some());
    }

    /// `\n`/`\r` in slug/guard/workdir/env keys+values would splice
    /// extra lines into the rendered task file — every field is
    /// refused (design.md §3.13; TOML escapes make these real).
    #[test]
    fn meta_rejects_embedded_newlines() {
        use super::{validate_meta, TaskMeta};
        let base = TaskMeta { id: None, slug: "t".into(), guard: None, workdir: None, env: Default::default(), cmd: vec!["true".into()], timeout_s: None, retry: None, retry_delay_s: None, gpu_mem_mb: None, ram_mb: None, cpu_cores: None };
        assert!(validate_meta(&base).is_ok());

        let mut m = base.clone();
        m.slug = "bad\nslug".into();
        assert!(validate_meta(&m).is_err());

        let mut m = base.clone();
        m.guard = Some("g\r\nwhoami".into());
        assert!(validate_meta(&m).is_err());

        let mut m = base.clone();
        m.workdir = Some("/x\nrm -rf /".into());
        assert!(validate_meta(&m).is_err());

        let mut m = base.clone();
        m.env.insert("K\nBAD".into(), "v".into());
        assert!(validate_meta(&m).is_err());

        let mut m = base.clone();
        m.env.insert("K".into(), "v\nevil".into());
        assert!(validate_meta(&m).is_err());

        // `=` in a key can't survive the `# shunt-env:K=V` round-
        // trip — the runner would read it as a different key.
        let mut m = base;
        m.env.insert("K=X".into(), "v".into());
        assert!(validate_meta(&m).is_err());
    }

    /// `dedup_key` reads the first `# shunt:id=` in the comment head
    /// (shebang and indented comments pass through) and stops at the
    /// first code line — a directive below it is body, not head.
    #[test]
    fn dedup_key_reads_head_only() {
        use super::dedup_key;
        assert_eq!(dedup_key("#!/usr/bin/env bash\n# shunt:id=abc-1\nexec true\n").as_deref(), Some("abc-1"));
        assert_eq!(dedup_key("  # shunt:id= spaced-2 \nexec true\n").as_deref(), Some("spaced-2"));
        // First match wins — a stale embedded id never shadows the
        // injected delivery id above it.
        assert_eq!(dedup_key("# shunt:id=new\n# shunt:id=old\nx\n").as_deref(), Some("new"));
        assert_eq!(dedup_key("#!/usr/bin/env bash\nexec true\n# shunt:id=later\n"), None);
        assert_eq!(dedup_key("#!/usr/bin/env bash\nexec true\n"), None);
        assert_eq!(dedup_key("# shunt:id=\nx\n"), None);
    }
}
