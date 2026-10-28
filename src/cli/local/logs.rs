//! Log access: `logs`/`dlog` — finding task logs, tail slicing,
//! follow loops, and the daemon.log JSONL render.

use anyhow::Result;
use std::path::PathBuf;

use super::{local_root, wire};
use crate::cli::args::{LogsArgs, RootArgs};

/// Find task logs matching `pat` across running/, done/, failed/ —
/// entry dirs' `log` files plus any flat `*.log` leftovers (legacy
/// sidecars, `<clean>.interrupted.<ts>.log` sweep products).
pub(crate) fn find_logs(root: &std::path::Path, pat: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for track in ["running", "done", "failed"] {
        for name in crate::client::queue::list_entries(root, track).unwrap_or_default() {
            if name.contains(pat) {
                let log = crate::client::queue::piece_file(root, track, &name, "log");
                if log.exists() {
                    out.push(log);
                }
            }
        }
        if let Ok(rd) = std::fs::read_dir(root.join(track)) {
            for ent in rd.flatten() {
                let name = ent.file_name().to_string_lossy().into_owned();
                if ent.file_type().map(|t| t.is_file()).unwrap_or(false) && name.ends_with(".log") && name.contains(pat) {
                    out.push(ent.path());
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Wire argv for a remote `logs`: `logs [-n N] [-- pat]`.
/// The pattern goes only when present — an empty positional would
/// parse as `Some("")` on the far end and match every task log
/// instead of selecting the daemon log. `-f` never crosses: the
/// caller bails on follow before building this.
pub(crate) fn logs_wire_tail(a: &LogsArgs) -> Vec<String> {
    let mut tail: Vec<String> = vec!["logs".into()];
    if let Some(n) = a.tail {
        tail.push("-n".into());
        tail.push(n.to_string());
    }
    if let Some(pat) = &a.pattern {
        // `--` keeps a dash-leading pattern positional on the
        // remote instead of parsing as a flag.
        tail.push("--".into());
        tail.push(pat.clone());
    }
    tail
}

/// Read just enough of `path`'s tail for `tail_bytes` to slice the
/// last `n` lines: bounded chunks backward from EOF until the buffer
/// holds `n` line breaks, then slice the collected suffix. A
/// whole-file read paid O(log size) per request — logs only grow,
/// so every `logs -n`/`dlog` reread the head to print a short tail.
pub(crate) fn tail_file(path: &std::path::Path, n: usize) -> Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    const CHUNK: u64 = 64 * 1024;
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    if n == 0 {
        return Ok(Vec::new());
    }
    let mut tail: Vec<u8> = Vec::new();
    let mut covered = len;
    while covered > 0 {
        let start = covered.saturating_sub(CHUNK);
        f.seek(SeekFrom::Start(start))?;
        let mut chunk = vec![0u8; (covered - start) as usize];
        f.read_exact(&mut chunk)?;
        chunk.extend_from_slice(&tail);
        tail = chunk;
        covered = start;
        // tail_bytes scans bytes[..len - trailing_nl] for the n-th
        // newline from the end; once that many are buffered, more
        // head data cannot change the slice it returns.
        let region = tail.len() - usize::from(tail.last() == Some(&b'\n'));
        if tail[..region].iter().filter(|&&b| b == b'\n').count() >= n {
            break;
        }
    }
    Ok(tail_bytes(&tail, n).to_vec())
}

/// The last `n` newline-terminated lines of `bytes`. A trailing
/// newline is a terminator, not an empty extra line; fewer than
/// `n` lines returns the whole buffer.
pub(crate) fn tail_bytes(bytes: &[u8], n: usize) -> &[u8] {
    // `tail -n 0` prints nothing; the `seen == n` check below can only
    // fire after `seen += 1`, so n = 0 must early-return here.
    if n == 0 {
        return &[];
    }
    let mut end = bytes.len();
    if end > 0 && bytes[end - 1] == b'\n' {
        end -= 1;
    }
    let mut seen = 0;
    for (i, &b) in bytes[..end].iter().enumerate().rev() {
        if b == b'\n' {
            seen += 1;
            if seen == n {
                return &bytes[i + 1..];
            }
        }
    }
    bytes
}

pub(crate) fn print_file(path: &std::path::Path) {
    use std::io::Write;
    match std::fs::read(path) {
        Ok(bytes) => {
            let _ = std::io::stdout().write_all(&bytes);
        }
        Err(e) => eprintln!("shunt: cannot read {}: {e}", path.display()),
    }
}

pub(crate) fn follow_file(path: &std::path::Path) -> Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::End(0))?;
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let n = f.read_to_end(&mut buf)?;
        if n > 0 {
            use std::io::Write;
            let _ = std::io::stdout().write_all(&buf);
            let _ = std::io::stdout().flush();
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

pub(crate) fn agent_logs(a: &LogsArgs) -> Result<()> {
    let root = local_root(&a.root)?;
    match &a.pattern {
        None => {
            agent_dlog(&RootArgs { root: a.root.clone(), json: false }, a.tail)?;
            if a.follow {
                follow_dlog(&root.join("daemon.log"))?;
            }
            Ok(())
        }
        Some(pat) => {
            let m = find_logs(&root, pat);
            match m.len() {
                0 => anyhow::bail!("no task log matches '{pat}'"),
                1 => {
                    match a.tail {
                        Some(n) => {
                            use std::io::Write;
                            let _ = std::io::stdout().write_all(&tail_file(&m[0], n)?);
                        }
                        None => print_file(&m[0]),
                    }
                    if a.follow {
                        follow_file(&m[0])?;
                    }
                    Ok(())
                }
                _ => {
                    eprintln!("pattern matches several logs, refusing:");
                    for p in &m {
                        eprintln!("  {}", p.display());
                    }
                    anyhow::bail!("ambiguous log pattern '{pat}'")
                }
            }
        }
    }
}

/// Render one daemon.log JSONL line as `[ts] event k=v`.
/// Non-JSON lines pass through verbatim — the dlog contract for
/// lines written before the format settled.
pub(super) fn render_dlog_line(line: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(line) {
        Ok(v) if v.is_object() => {
            let ts = v.get("ts").and_then(|t| t.as_str()).unwrap_or("");
            let ev = v.get("event").and_then(|t| t.as_str()).unwrap_or("");
            let rest: Vec<String> = v.as_object().map(|o| o.iter().filter(|(k, _)| k.as_str() != "ts" && k.as_str() != "event").map(|(k, v)| format!("{k}={v}")).collect()).unwrap_or_default();
            format!("[{ts}] {ev} {}", rest.join(" "))
        }
        _ => line.to_string(),
    }
}

/// `tail -f` on daemon.log, rendering each appended line through
/// `render_dlog_line` so `logs -f` looks like `dlog`, not raw JSON.
/// An unterminated tail-of-file fragment is held until its newline
/// arrives so a half-written record never renders mid-line.
fn follow_dlog(path: &std::path::Path) -> Result<()> {
    use std::io::{Read, Seek, SeekFrom, Write};
    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::End(0))?;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let mut chunk = Vec::new();
        if f.read_to_end(&mut chunk)? > 0 {
            buf.extend_from_slice(&chunk);
            while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
                let line = String::from_utf8_lossy(&buf[..nl]).into_owned();
                buf.drain(..=nl);
                println!("{}", render_dlog_line(&line));
            }
            let _ = std::io::stdout().flush();
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

pub(crate) fn agent_dlog(a: &RootArgs, tail: Option<usize>) -> Result<()> {
    wire(a.json, dlog_impl(a, tail))
}

fn dlog_impl(a: &RootArgs, tail: Option<usize>) -> Result<()> {
    let root = local_root(&a.root)?;
    let path = root.join("daemon.log");
    // Render JSONL human-readable: `[ts] event k=v`. `--json` skips
    // the render — daemon.log IS JSONL, so the wire form is the file
    // verbatim (still tail-sliced). A bounded tail reads backward
    // from EOF; the daemon log only grows, so rereading its head per
    // request would scale O(log size) for a 200-line answer.
    let text = match tail {
        Some(n) => tail_file(&path, n).ok().and_then(|b| String::from_utf8(b).ok()),
        None => std::fs::read_to_string(&path).ok(),
    };
    if let Some(text) = text {
        for line in text.lines() {
            if a.json {
                println!("{line}");
            } else {
                println!("{}", render_dlog_line(line));
            }
        }
    }
    Ok(())
}
