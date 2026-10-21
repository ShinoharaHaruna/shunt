//! Sensitive-value scrub (design.md §14.2). The default repo is
//! public, so every report passes through here before it is
//! persisted or delivered: the realistic leak is an agent pasting a
//! `ssh root@1.2.3.4 -p 12345` error verbatim into the body.

use std::fs;

/// Values that must never reach a public issue: every client row's
/// ssh fields plus the $HOME prefix. Rows are read as raw TOML —
/// typed loading is deliberately bypassed because a disabled or
/// unparseable row still holds secrets, and `load_client` rejects
/// exactly those.
pub fn secrets() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Ok(h) = std::env::var("HOME") {
        out.push(h);
    }
    if let Ok(dir) = crate::server::registry::shunt_home().map(|h| h.join("clients")) {
        if let Ok(rd) = fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().and_then(|s| s.to_str()) != Some("toml") {
                    continue;
                }
                let Ok(v) = fs::read_to_string(&p).map(|t| t.parse::<toml::Value>()) else { continue };
                let Ok(v) = v else { continue };
                let Some(ssh) = v.get("ssh").and_then(|t| t.as_table()) else { continue };
                for k in ["host", "user", "key", "password"] {
                    if let Some(s) = ssh.get(k).and_then(|v| v.as_str()) {
                        out.push(s.to_string());
                    }
                }
                if let Some(p) = ssh.get("port").and_then(|v| v.as_integer()) {
                    out.push(p.to_string());
                }
            }
        }
    }
    // Longest-first so a prefix value ($HOME) cannot partially eat a
    // longer one (a key path under $HOME) before it runs.
    out.sort_by_key(|s| std::cmp::Reverse(s.len()));
    out.dedup();
    out
}

/// Usernames etc. that are simultaneously common words — redacting
/// "root" would mangle "root cause". A genuinely unusual login name
/// still gets scrubbed (it clears the length bar and isn't listed).
const ALLOWLIST: &[&str] = &["root", "ubuntu", "debian", "admin", "user", "git", "shunt", "localhost"];

/// Replace every occurrence of each secret with `<redacted>`.
/// Values shorter than 4 chars are skipped: they collide with
/// ordinary prose (port "22" would nuke every "22" in the body).
/// Returns (scrubbed text, replacements made).
pub fn scrub(text: &str, secrets: &[String]) -> (String, usize) {
    let mut out = text.to_string();
    let mut n = 0usize;
    for s in secrets {
        if s.len() < 4 || ALLOWLIST.contains(&s.as_str()) {
            continue;
        }
        let c = out.matches(s.as_str()).count();
        if c > 0 {
            out = out.replace(s.as_str(), "<redacted>");
            n += c;
        }
    }
    (out, n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secrets(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn scrub_redacts_known_secrets() {
        let (out, n) = scrub("ssh root@203.0.113.7 -p 12345 failed; key /home/u/.config/shunt/keys/rw-a", &secrets(&["203.0.113.7", "12345", "/home/u"]));
        assert!(!out.contains("203.0.113.7"));
        assert!(!out.contains("12345"));
        assert_eq!(out, "ssh root@<redacted> -p <redacted> failed; key <redacted>/.config/shunt/keys/rw-a");
        assert_eq!(n, 3);
    }

    #[test]
    fn scrub_skips_short_and_allowlisted() {
        // "root" stays (allowlist), "22" stays (< 4 chars).
        let (out, n) = scrub("login root on port 22 hit root cause", &secrets(&["root", "22"]));
        assert_eq!(out, "login root on port 22 hit root cause");
        assert_eq!(n, 0);
    }

    #[test]
    fn scrub_unusual_user_redacted() {
        let (out, n) = scrub("ssh shinohara@box failed", &secrets(&["shinohara"]));
        assert_eq!(out, "ssh <redacted>@box failed");
        assert_eq!(n, 1);
    }
}
