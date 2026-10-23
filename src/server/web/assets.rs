//! Embedded `web/dist` (build.rs emits the table into OUT_DIR).
//! The binary is the deployment unit — the UI travels with it.

include!(concat!(env!("OUT_DIR"), "/web_assets.rs"));

/// Resolve a request path to embedded bytes + content type.
/// `/` serves index.html; everything else exact-matches the dist
/// tree, so there is no traversal surface to guard.
pub fn lookup(path: &str) -> Option<(&'static [u8], &'static str)> {
    let key = path.strip_prefix('/').unwrap_or(path);
    let key = if key.is_empty() { "index.html" } else { key };
    WEB_ASSETS.iter().find(|(p, _)| *p == key).map(|(p, b)| (*b, mime(p)))
}

fn mime(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("json" | "map") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("ttf") => "font/ttf",
        _ => "application/octet-stream",
    }
}
