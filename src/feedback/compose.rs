//! Issue body composition (design.md §14.2): report text, a fenced
//! JSON context block (machine-readable — same truth an agent can
//! parse), and a provenance footer.

use anyhow::Result;

/// GitHub rejects issue bodies past 64 KiB — an oversized report
/// would persist pending forever, so the report text is truncated
/// ahead of that bound with an explicit marker (silent truncation
/// would hide that content was dropped).
const MAX_REPORT_BYTES: usize = 60_000;

/// `<body>` + context block + footer. `--client` contributes the
/// row's facts/billing/expire — never `ssh.*`, and no network: the
/// row is read off disk.
pub fn body(report: &str, client: Option<&str>) -> Result<String> {
    let report = if report.len() > MAX_REPORT_BYTES {
        // char-boundary safe: reports are text; step back to the
        // nearest boundary rather than slicing mid-UTF-8.
        let mut end = MAX_REPORT_BYTES;
        while !report.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}\n\n… [truncated: report exceeded {MAX_REPORT_BYTES} bytes]", &report[..end])
    } else {
        report.to_string()
    };
    let report = report.as_str();
    // SHUNT_VERSION carries the build sha: `0.2.0 (abc1234 …)` —
    // the exact code that ran, which is the point of the context.
    let mut ctx = serde_json::json!({
        "shunt": env!("SHUNT_VERSION"),
        "protocol": crate::proto::wire::PROTOCOL_VERSION,
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
    });
    if let Some(name) = client {
        let row = crate::server::registry::load_client(name)?;
        // Only fields that cannot carry credentials: ssh.* and
        // key/workdir paths stay out by construction.
        ctx["client"] = serde_json::json!({
            "name": row.name,
            "facts": row.facts,
            "billing": row.billing,
            "expire": row.expire,
        });
    }
    Ok(format!("{report}\n\n---\n```json\n{}\n```\n\nfiled by `shunt feedback`", serde_json::to_string_pretty(&ctx)?))
}
