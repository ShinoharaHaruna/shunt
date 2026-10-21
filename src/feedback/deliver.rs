//! GitHub issue delivery (design.md §14.2): `gh` first, then
//! `GH_TOKEN`/`GITHUB_TOKEN` + curl. Both are std::process
//! shell-outs — no HTTP client dep enters the tree for this.

use anyhow::{Context, Result};
use std::io::Write;
use std::process::{Command, Stdio};

/// Compile-time upstream default; a fork overrides it via
/// `server.toml` `feedback_repo` or the env, not by patching source.
pub const DEFAULT_REPO: &str = "ShinoharaHaruna/shunt";

pub struct Delivered {
    pub url: String,
    pub number: u64,
}

/// `$SHUNT_FEEDBACK_REPO` > `server.toml feedback_repo` > default.
/// An empty env value disables delivery deliberately (test hook and
/// user opt-out). A corrupt server.toml warns and falls through to
/// the default — the report might be about that very file.
pub fn resolve_repo() -> Result<String> {
    match std::env::var("SHUNT_FEEDBACK_REPO") {
        Ok(v) => {
            let v = v.trim();
            anyhow::ensure!(!v.is_empty(), "feedback delivery disabled: SHUNT_FEEDBACK_REPO is empty (unset it to enable)");
            Ok(v.to_string())
        }
        Err(_) => match crate::server::registry::load_server_config() {
            Ok(c) => Ok(c.feedback_repo.unwrap_or_else(|| DEFAULT_REPO.into())),
            Err(e) => {
                eprintln!("warning: server.toml unreadable ({e:#}); using default repo {DEFAULT_REPO}");
                Ok(DEFAULT_REPO.into())
            }
        },
    }
}

/// Try each mechanism in order; report all failures together so the
/// operator sees the whole picture (gh missing AND no token, not
/// just the first miss).
pub fn deliver(repo: &str, title: &str, body: &str) -> Result<Delivered> {
    let mut errs = Vec::new();
    match gh(repo, title, body) {
        Ok(d) => return Ok(d),
        Err(e) => errs.push(format!("gh: {e:#}")),
    }
    match curl(repo, title, body) {
        Ok(d) => return Ok(d),
        Err(e) => errs.push(format!("curl: {e:#}")),
    }
    anyhow::bail!("cannot deliver to github ({}) — install gh or export GH_TOKEN", errs.join("; "))
}

/// `gh issue create --repo R --title T --body-file -` — the CLI's
/// own auth applies; no token handling here. The URL is the last
/// https line on stdout.
fn gh(repo: &str, title: &str, body: &str) -> Result<Delivered> {
    let out = run(Command::new("gh").args(["issue", "create", "--repo", repo, "--title", title, "--body-file", "-"]), body).context("gh not runnable")?;
    anyhow::ensure!(out.status.success(), "gh issue create failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    let url = String::from_utf8_lossy(&out.stdout).lines().rev().find(|l| l.trim().starts_with("http")).map(|l| l.trim().to_string()).context("gh printed no issue URL")?;
    let number = url.rsplit('/').next().and_then(|n| n.parse::<u64>().ok()).unwrap_or(0);
    Ok(Delivered { url, number })
}

/// `curl -d @- https://api.github.com/repos/R/issues` with a token
/// from env. `--max-time` bounds the call — a feedback post must
/// never hang the caller.
///
/// The token rides in a 0600 curl config file (`-K`), never argv:
/// the registry keeps secrets at 0600 while process argv is
/// world-readable via /proc — a Bearer header in args would leak.
fn curl(repo: &str, title: &str, body: &str) -> Result<Delivered> {
    let token = std::env::var("GH_TOKEN").ok().or_else(|| std::env::var("GITHUB_TOKEN").ok()).context("GH_TOKEN/GITHUB_TOKEN unset")?;
    let payload = serde_json::json!({"title": title, "body": body}).to_string();

    // Config under the feedback dir (already 0700); falls back to
    // the system temp dir when the ledger dir itself is unavailable.
    let cfg_dir = super::ledger::dir().unwrap_or_else(|_| std::env::temp_dir());
    let cfg = cfg_dir.join(format!(".curl-{}-{}.cfg", std::process::id(), super::now()));
    crate::server::fsutil::write_private(&cfg, format!("header = \"Authorization: Bearer {token}\"\n"))?;
    let out = run(Command::new("curl").args(["-sS", "--max-time", "30", "-X", "POST", "-K"]).arg(&cfg).args(["-H", "Accept: application/vnd.github+json", "-H", "X-GitHub-Api-Version: 2022-11-28", "--data-binary", "@-", &format!("https://api.github.com/repos/{repo}/issues")]), &payload);
    let _ = std::fs::remove_file(&cfg);
    let out = out.context("curl not runnable")?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).context("unparseable github response")?;
    anyhow::ensure!(out.status.success() && v.get("html_url").is_some(), "github api error: {}", v.get("message").and_then(|m| m.as_str()).unwrap_or("unknown"));
    Ok(Delivered { url: v["html_url"].as_str().unwrap_or_default().to_string(), number: v["number"].as_u64().unwrap_or(0) })
}

/// Spawn with `body` on stdin, collect output. A missing binary is
/// reported, not crashed through.
fn run(cmd: &mut Command, body: &str) -> Result<std::process::Output> {
    let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().with_context(|| format!("spawn failed: {cmd:?}"))?;
    if let Some(mut s) = child.stdin.take() {
        let _ = s.write_all(body.as_bytes());
    }
    child.wait_with_output().context("wait failed")
}
