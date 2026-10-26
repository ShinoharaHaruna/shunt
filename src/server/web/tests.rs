//! Gates and routing for `serve` (design.md §18): the Host
//! allowlist, the same-origin gate, and the two POST bodies that
//! must never smuggle a command string.

use super::guard::{loopback_host_allowed, same_origin_ok};
use super::http;
use super::routes::{handle, ProbeRequest};
use super::{snapshot, ws, Latest};
use serde_json::json;
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, RwLock};

fn request(headers: &[(&str, &str)]) -> http::Request {
    http::Request { method: "GET".into(), path: "/".into(), query: String::new(), headers: headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(), body: Vec::new() }
}

#[test]
fn same_origin_gate_accepts_same_origin_and_non_browser_requests() {
    assert!(same_origin_ok(&request(&[("host", "LOCALHOST:8747"), ("origin", "http://localhost:8747")])));
    assert!(same_origin_ok(&request(&[("sec-fetch-site", "none")])));
    assert!(same_origin_ok(&request(&[])));
}

#[test]
fn same_origin_gate_rejects_cross_site_metadata_and_origins() {
    assert!(!same_origin_ok(&request(&[("sec-fetch-site", "cross-site")])));
    assert!(!same_origin_ok(&request(&[("sec-fetch-site", "same-site")])));
    assert!(!same_origin_ok(&request(&[("host", "localhost:8747"), ("origin", "null")])));
    assert!(!same_origin_ok(&request(&[("host", "localhost:8747"), ("origin", "http://evil.example")])));
}

#[test]
fn loopback_host_gate_accepts_local_names_and_rejects_rebinding() {
    for host in ["127.0.0.1", "127.0.0.1:8747", "localhost", "LOCALHOST.:8747", "[::1]", "[::1]:8747"] {
        assert!(loopback_host_allowed(Some(host), "127.0.0.1"), "{host}");
    }
    assert!(loopback_host_allowed(Some("127.0.1.1:8747"), "127.0.1.1"));
    assert!(!loopback_host_allowed(Some("attacker.example:8747"), "127.0.0.1"));
    assert!(!loopback_host_allowed(Some("localhost.example:8747"), "127.0.0.1"));
    assert!(!loopback_host_allowed(None, "127.0.0.1"));
}

fn response_for(raw: &str, bind: &str) -> String {
    use std::io::{Read, Write};

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let mut client = TcpStream::connect(addr).unwrap();
    client.write_all(raw.as_bytes()).unwrap();
    let (server, _) = listener.accept().unwrap();
    let latest: Latest = Arc::new(RwLock::new(json!({})));
    handle(server, &latest, &ws::Hub::new(), &None, bind).unwrap();

    let mut response = String::new();
    client.read_to_string(&mut response).unwrap();
    response
}

#[test]
fn loopback_request_rejects_rebinding_host_before_routing() {
    let response = response_for("GET /api/snapshot HTTP/1.1\r\nHost: attacker.example:8747\r\nOrigin: http://attacker.example:8747\r\nSec-Fetch-Site: same-origin\r\n\r\n", "127.0.0.1");
    assert!(response.starts_with("HTTP/1.1 403 Forbidden\r\n"));
    assert!(response.contains("invalid Host for loopback bind"));
}

#[test]
fn probe_request_ignores_untrusted_command_fields() {
    let body: ProbeRequest = serde_json::from_str(r#"{"client":"localhost","probe":"ps","cmd":"rm -rf /"}"#).unwrap();
    assert_eq!(body.client, "localhost");
    assert_eq!(snapshot::probe_argv(&body.probe), Some(&["ps", "-eo", "pid,pgid,etime,%cpu,stat,cmd", "--sort=-%cpu"][..]));
    assert_eq!(snapshot::probe_argv("rm -rf /"), None);
}

#[test]
fn unknown_probe_is_a_bad_request_listing_valid_names() {
    let body = r#"{"client":"localhost","probe":"unknown"}"#;
    let raw = format!("POST /api/probe HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{body}", body.len());
    let response = response_for(&raw, "127.0.0.1");
    assert!(response.starts_with("HTTP/1.1 400 Bad Request\r\n"));
    assert!(response.contains("unknown probe \\\"unknown\\\"; valid probes: ps, gpu, df, mem, uptime"));
}

/// The "probe now" route is a status query, not a command: an empty
/// or absent client is refused before anything touches the registry.
#[test]
fn reprobe_requires_a_client_and_never_runs_a_command() {
    let body = r#"{"client":""}"#;
    let raw = format!("POST /api/reprobe HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{body}", body.len());
    let response = response_for(&raw, "127.0.0.1");
    assert!(response.starts_with("HTTP/1.1 400 Bad Request\r\n"));
    assert!(response.contains("expected {client}"));
}
