//! Sensitive-value scrub (design.md §14.2). The default repo is
//! public, so every report passes through here before it is
//! persisted or delivered: the realistic leaks are an agent pasting
//! a `ssh root@1.2.3.4 -p 12345` error verbatim into the body, and
//! registered names (projects, boxes, queues) appearing verbatim in
//! titles and bodies.

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

/// Registered names paired with the placeholder each redacts to:
/// client names (`clients/*.toml` stems — disabled rows still list,
/// a parked box's name is still sensitive) → `<BOX_NAME>`, project
/// names (`projects/*.toml` stems) → `<PROJECT_NAME>`, queue names
/// (each row's `[queues.<name>]` tables) → `<QUEUE_NAME>`.
///
/// The set is read from the live registry at compose/retry time —
/// a name deleted beforehand cannot be redacted, which is
/// acceptable: the registry is the truth of what exists. Queue
/// names load via `load_client_row`, not `load_client`: typed
/// loading would drop disabled rows (their names are no less
/// sensitive) and materialize `clients/localhost.toml` on disk —
/// two side effects a read-only scrub does not want. A row that
/// fails to parse just contributes no queues.
pub fn names() -> Vec<(String, &'static str)> {
    let mut out: Vec<(String, &'static str)> = Vec::new();
    if let Ok(clients) = crate::server::registry::list_names("clients") {
        for name in &clients {
            out.push((name.clone(), "<BOX_NAME>"));
        }
        for name in &clients {
            if let Ok(row) = crate::server::registry::load_client_row(name) {
                out.extend(row.queues.into_keys().map(|q| (q, "<QUEUE_NAME>")));
            }
        }
    }
    if let Ok(projects) = crate::server::registry::list_names("projects") {
        out.extend(projects.into_iter().map(|p| (p, "<PROJECT_NAME>")));
    }
    // Longest-first across all kinds: a short name that prefixes a
    // longer one ("gpu" vs "gpu-1") must not eat it partially.
    out.sort_by_key(|p| std::cmp::Reverse(p.0.len()));
    out.dedup();
    out
}

/// Reserved/generic names exempt from redaction: `default` is the
/// canonical queue name every flat row resolves to (and plain
/// prose), and the shared allowlist covers `localhost`/`admin`/etc.
/// — `localhost` names the loopback peer, not a real box, so
/// scrubbing it would mangle legitimate prose while exposing
/// nothing. Matched case-insensitively: `LocalHost` is as generic.
fn exempt_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("default") || ALLOWLIST.iter().any(|w| name.eq_ignore_ascii_case(w))
}

/// Case-insensitive substring pass over registered names — a
/// mixed-case hit redacts regardless of how it was typed, and an
/// embedded name keeps its suffix (`riboworld-waves` →
/// `<PROJECT_NAME>-waves`). Single-character names are skipped:
/// a lone letter is indistinguishable from prose — redacting it
/// would destroy the report while removing no real signal.
/// Returns (scrubbed text, replacements made).
pub fn scrub_names(text: &str, names: &[(String, &str)]) -> (String, usize) {
    let mut out = text.to_string();
    let mut n = 0usize;
    for (name, placeholder) in names {
        if name.len() < 2 || exempt_name(name) {
            continue;
        }
        let (next, c) = replace_ci(&out, name, placeholder);
        if c > 0 {
            out = next;
            n += c;
        }
    }
    (out, n)
}

/// Case-insensitive literal replace, std-only. `needle` is a
/// registry name — ASCII `[A-Za-z0-9_.-]` — so ASCII case-folding
/// on byte windows suffices: a match can only start on an ASCII
/// byte, which is always a UTF-8 boundary, so slicing `text` at
/// match edges is safe. Non-overlapping, left to right, like
/// `str::replace`. Returns (new text, replacements made).
fn replace_ci(text: &str, needle: &str, replacement: &str) -> (String, usize) {
    if needle.is_empty() {
        return (text.to_string(), 0);
    }
    let (bytes, needle) = (text.as_bytes(), needle.as_bytes());
    let mut out = String::with_capacity(text.len());
    let (mut count, mut i, mut last) = (0usize, 0usize, 0usize);
    while i + needle.len() <= bytes.len() {
        if bytes[i..i + needle.len()].eq_ignore_ascii_case(needle) {
            out.push_str(&text[last..i]);
            out.push_str(replacement);
            i += needle.len();
            last = i;
            count += 1;
        } else {
            i += 1;
        }
    }
    out.push_str(&text[last..]);
    (out, count)
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

    fn pairs(v: &[(&'static str, &'static str)]) -> Vec<(String, &'static str)> {
        v.iter().map(|(n, p)| (n.to_string(), *p)).collect()
    }

    #[test]
    fn names_reads_each_registry_domain() {
        let _g = crate::server::registry::tests::HOME_LOCK.lock().unwrap();
        let home = std::env::temp_dir().join(format!("shunt-names-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(home.join("clients")).unwrap();
        fs::create_dir_all(home.join("projects")).unwrap();
        fs::write(home.join("clients/pytorch-7032.toml"), "name = \"pytorch-7032\"\ndeploy_root = \"/root\"\n[queues.gpu-queue]\nqroot = \"/root/q/gpu\"\n").unwrap();
        // A disabled row's names are still sensitive.
        fs::write(home.join("clients/parked-box.toml"), "name = \"parked-box\"\ndeploy_root = \"/root\"\ndisabled = true\n[queues.side]\nqroot = \"/root/q/side\"\n").unwrap();
        fs::write(home.join("projects/RiboWorld.toml"), "name = \"RiboWorld\"\n").unwrap();
        std::env::set_var("SHUNT_HOME", &home);
        let names = names();
        std::env::remove_var("SHUNT_HOME");
        fs::remove_dir_all(&home).unwrap();
        for want in [("pytorch-7032", "<BOX_NAME>"), ("parked-box", "<BOX_NAME>"), ("gpu-queue", "<QUEUE_NAME>"), ("side", "<QUEUE_NAME>"), ("RiboWorld", "<PROJECT_NAME>")] {
            assert!(names.contains(&(want.0.to_string(), want.1)), "missing {want:?} in {names:?}");
        }
        // Longest-first: "pytorch-7032" precedes any prefix of it.
        let pos = |n: &str| names.iter().position(|(name, _)| name == n).unwrap();
        assert!(pos("pytorch-7032") < pos("side"), "{names:?}");
    }

    #[test]
    fn scrub_names_redacts_each_kind_case_insensitively() {
        let names = pairs(&[("pytorch-7032", "<BOX_NAME>"), ("RiboWorld", "<PROJECT_NAME>"), ("gpu-queue", "<QUEUE_NAME>")]);
        let (out, n) = scrub_names("RIBOWORLD ran on PyTorch-7032 via GPU-QUEUE", &names);
        assert_eq!(out, "<PROJECT_NAME> ran on <BOX_NAME> via <QUEUE_NAME>");
        assert_eq!(n, 3);
    }

    #[test]
    fn scrub_names_longest_first_keeps_embeds_and_suffixes() {
        // "gpu" prefixes "gpu-1": sorted longest-first by names(),
        // the longer name redacts whole rather than leaving "-1".
        let mut names = pairs(&[("gpu", "<BOX_NAME>"), ("gpu-1", "<BOX_NAME>"), ("RiboWorld", "<PROJECT_NAME>")]);
        names.sort_by_key(|p| std::cmp::Reverse(p.0.len()));
        let (out, n) = scrub_names("GPU-1 down; riboworld-waves up", &names);
        assert_eq!(out, "<BOX_NAME> down; <PROJECT_NAME>-waves up");
        assert_eq!(n, 2);
    }

    #[test]
    fn scrub_names_exempt_and_single_char_pass_through() {
        let names = pairs(&[("localhost", "<BOX_NAME>"), ("Admin", "<BOX_NAME>"), ("default", "<QUEUE_NAME>"), ("q", "<QUEUE_NAME>")]);
        let (out, n) = scrub_names("localhost stays, Admin stays, default stays, q stays", &names);
        assert_eq!(out, "localhost stays, Admin stays, default stays, q stays");
        assert_eq!(n, 0);
    }
}
