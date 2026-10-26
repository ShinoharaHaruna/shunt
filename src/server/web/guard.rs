//! Request gates for `serve` (design.md §18), run before routing:
//! the loopback bind guard, the Host allowlist that blocks DNS
//! rebinding, the same-origin metadata gate, and token auth. Every
//! rule here is a refusal, not a fallback — a request that fails one
//! never reaches a route.

use super::flags::ServeOpts;
use super::http;
use anyhow::Result;

pub fn is_loopback_bind(bind: &str) -> bool {
    matches!(bind, "127.0.0.1" | "127.0.1.1" | "::1" | "localhost")
}

pub fn ensure_bind_allowed(opts: &ServeOpts) -> Result<()> {
    let loopback = is_loopback_bind(&opts.bind);
    anyhow::ensure!(loopback || opts.token.is_some(), "refusing to bind {}:{} without --token — the dashboard exposes fleet state to anyone who can reach the port; bind 127.0.0.1 (and use `ssh -L {0}:{1}:127.0.0.1:{1} <dev>` from your browser) or pass --token", opts.bind, opts.port);
    Ok(())
}

fn host_component(host: &str) -> Option<&str> {
    let host = host.trim();
    if let Some(rest) = host.strip_prefix('[') {
        let (component, suffix) = rest.split_once(']')?;
        component.parse::<std::net::Ipv6Addr>().ok()?;
        if !suffix.is_empty() && !suffix.strip_prefix(':').is_some_and(|port| port.parse::<u16>().is_ok()) {
            return None;
        }
        return Some(component);
    }
    match host.rsplit_once(':') {
        Some((component, port)) if !component.contains(':') => {
            port.parse::<u16>().ok()?;
            Some(component)
        }
        Some(_) => None,
        None => Some(host),
    }
}

/// A loopback listener must not accept an attacker-controlled Host:
/// DNS rebinding keeps Host and Origin equal while resolving to 127.0.0.1.
pub fn loopback_host_allowed(host: Option<&str>, bind: &str) -> bool {
    let Some(host) = host.and_then(host_component) else { return false };
    let host = host.strip_suffix('.').unwrap_or(host);
    ["127.0.0.1", "localhost", "::1", bind].iter().any(|allowed| host.eq_ignore_ascii_case(allowed))
}

fn origin_authority(origin: &str) -> Option<&str> {
    let origin = origin.trim();
    if origin.eq_ignore_ascii_case("null") {
        return None;
    }
    let (scheme, authority) = origin.split_once("://")?;
    if !(scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")) || authority.is_empty() || authority.bytes().any(|b| b.is_ascii_whitespace() || matches!(b, b'/' | b'?' | b'#' | b'@' | b',')) {
        return None;
    }
    Some(authority)
}

/// Browser metadata blocks cross-site reads and writes before routing
/// or token checks; absent headers keep non-browser clients usable.
pub fn same_origin_ok(req: &http::Request) -> bool {
    if req.header("sec-fetch-site").is_some_and(|site| !site.eq_ignore_ascii_case("same-origin") && !site.eq_ignore_ascii_case("none")) {
        return false;
    }
    let Some(origin) = req.header("origin") else {
        return true;
    };
    let Some(host) = req.header("host") else {
        return false;
    };
    origin_authority(origin).is_some_and(|authority| authority.eq_ignore_ascii_case(host))
}

pub fn authorized(req: &http::Request, token: &Option<String>) -> bool {
    let Some(expected) = token else {
        return true;
    };
    let presented = http::query_param(&req.query, "token").or_else(|| req.header("x-shunt-token").map(str::to_string)).or_else(|| req.header("authorization").and_then(|a| a.strip_prefix("Bearer ").map(str::to_string)));
    presented.as_deref() == Some(expected.as_str())
}
