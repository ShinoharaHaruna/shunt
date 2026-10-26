//! Minimal HTTP/1.1 surface for `serve` (design.md §18): enough to
//! route GETs, one bounded probe POST, and detect a WebSocket upgrade.
//! Not a general server — every response closes the connection.

use anyhow::Result;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// Request headers past this many bytes mean the peer is not a
/// browser asking for a page — bounded so a garbage peer cannot
/// grow our buffer without limit.
const MAX_HEAD: usize = 32 * 1024;

pub struct Request {
    pub method: String,
    /// Path without the query string.
    pub path: String,
    /// Raw query string, no leading `?` ("" when absent).
    pub query: String,
    /// Header names lowercased; values verbatim.
    pub headers: Vec<(String, String)>,
    /// Request body, read per Content-Length (empty for GETs).
    /// Bounded: the only body route selects a fixed probe and its
    /// request never needs more than this.
    pub body: Vec<u8>,
}

const MAX_BODY: usize = 16 * 1024;

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

/// Read one request head (up to the blank line), then the body
/// per Content-Length. `None` on a closed/garbage/overlong head —
/// the caller just drops the conn.
pub fn read_request(stream: &mut TcpStream) -> Option<Request> {
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok()?;
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(pos) = find(&buf, b"\r\n\r\n") {
            break pos;
        }
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > MAX_HEAD {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut body = buf.split_off(head_end + 4);
    let mut lines = head.split("\r\n");
    let mut req = lines.next()?.split_whitespace();
    let method = req.next()?.to_string();
    let uri = req.next()?;
    let (path, query) = match uri.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (uri.to_string(), String::new()),
    };
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            Some((k.trim().to_ascii_lowercase(), v.trim().to_string()))
        })
        .collect();
    let want = headers.iter().find(|(k, _)| k == "content-length").and_then(|(_, v)| v.parse::<usize>().ok()).unwrap_or(0).min(MAX_BODY);
    while body.len() < want {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n.min(want - body.len())]);
    }
    body.truncate(want);
    Some(Request { method, path, query, headers, body })
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// One query parameter's value, percent-decoded enough for tokens
/// (alnum + `._~-` pass through; %XX decoded).
pub fn query_param(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if k == key {
            return Some(percent_decode(v));
        }
    }
    None
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Write a response and return; callers always close afterwards.
/// `extra` carries per-route headers (CSP, cache policy).
pub fn respond(stream: &mut TcpStream, code: u16, reason: &str, content_type: &str, body: &[u8], extra: &[(&str, &str)]) -> Result<()> {
    let mut head = format!("HTTP/1.1 {code} {reason}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n", body.len());
    for (k, v) in extra {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;
    Ok(())
}

pub fn json(stream: &mut TcpStream, code: u16, body: &serde_json::Value) -> Result<()> {
    let reason = match code {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        _ => "?",
    };
    respond(stream, code, reason, "application/json; charset=utf-8", serde_json::to_string(body)?.as_bytes(), &[("cache-control", "no-store")])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_param_decoding() {
        assert_eq!(query_param("token=ab%20c", "token").as_deref(), Some("ab c"));
        assert_eq!(query_param("a=1&b=2", "b").as_deref(), Some("2"));
        assert_eq!(query_param("a=1", "zz"), None);
        // Truncated percent escapes pass through literally.
        assert_eq!(query_param("t=%2", "t").as_deref(), Some("%2"));
    }

    #[test]
    fn head_parsing_over_a_socket() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::thread::spawn(move || {
            let mut c = TcpStream::connect(addr).unwrap();
            c.write_all(b"GET /ws?token=abc HTTP/1.1\r\nHost: x\r\nSec-WebSocket-Key: k\r\n\r\n").unwrap();
            // Keep the socket open until the server finishes reading.
            std::thread::sleep(Duration::from_millis(50));
        });
        let (mut s, _) = listener.accept().unwrap();
        let req = read_request(&mut s).unwrap();
        assert_eq!(req.method, "GET");
        assert_eq!(req.path, "/ws");
        assert_eq!(req.query, "token=abc");
        assert_eq!(req.header("sec-websocket-key"), Some("k"));
        client.join().unwrap();
    }
}
