//! WebSocket fan-out: one `Hub`, one reader/writer pair per conn.
//! tungstenite does the framing; the streams are std TcpStreams,
//! so the reader/writer split is two `try_clone`d fds sharing one
//! socket — TCP full-duplex keeps the byte directions disjoint.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::io::Write;
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use tungstenite::protocol::Role;
use tungstenite::{Message, WebSocket};

/// Connected dashboards. A `SyncSender` caps each conn's backlog:
/// a consumer that falls a full buffer behind gets dropped (it can
/// reconnect — the next snapshot catches it up), rather than
/// growing memory or stalling the broadcast for everyone.
#[derive(Clone, Default)]
pub struct Hub {
    inner: Arc<Mutex<HashMap<u64, SyncSender<String>>>>,
    next: Arc<AtomicU64>,
}

impl Hub {
    pub fn new() -> Self {
        Self::default()
    }

    /// The initial snapshot rides the same channel as later
    /// broadcasts, so a conn can never see an older state land
    /// after a newer one.
    fn register(&self, initial: String) -> (u64, Receiver<String>) {
        let (tx, rx) = sync_channel(64);
        let _ = tx.send(initial);
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.inner.lock().unwrap().insert(id, tx);
        (id, rx)
    }

    fn remove(&self, id: u64) {
        self.inner.lock().unwrap().remove(&id);
    }

    pub fn broadcast(&self, text: &str) {
        self.inner.lock().unwrap().retain(|_, tx| tx.try_send(text.to_string()).is_ok());
    }
}

/// Upgrade an already-parsed GET to a websocket, register the conn,
/// then read client frames until close/disconnect. The writer
/// thread drains the hub channel onto the socket.
pub fn upgrade(mut stream: TcpStream, req: &super::http::Request, hub: Hub, latest: &super::Latest) -> Result<()> {
    let is_ws = req.header("upgrade").is_some_and(|u| u.eq_ignore_ascii_case("websocket"));
    let key = req.header("sec-websocket-key");
    let key = if is_ws { key } else { None }.ok_or_else(|| anyhow::anyhow!("bad websocket upgrade request"))?;
    let accept = tungstenite::handshake::derive_accept_key(key.as_bytes());
    write!(stream, "HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\nconnection: upgrade\r\nsec-websocket-accept: {accept}\r\n\r\n").and_then(|()| stream.flush()).context("websocket handshake write")?;

    let initial = serde_json::to_string(&*latest.read().unwrap())?;
    let (id, rx) = hub.register(initial);

    // Writer half: the reader never writes back — a pong could
    // interleave mid-frame with an in-flight broadcast and corrupt
    // the stream. Browsers' WS API cannot ping, and the broadcast
    // cadence doubles as the liveness signal.
    let writer_stream = stream.try_clone()?;
    let ctl = stream.try_clone()?;
    let h2 = hub.clone();
    std::thread::spawn(move || {
        let mut ws_out = WebSocket::from_raw_socket(writer_stream, Role::Server, None);
        while let Ok(text) = rx.recv() {
            if ws_out.send(Message::Text(text.into())).is_err() {
                break;
            }
        }
        // The reader may be blocked in read() on the other fd —
        // shutdown wakes it so the conn cannot half-leak.
        let _ = ctl.shutdown(std::net::Shutdown::Both);
        h2.remove(id);
    });

    let mut ws_in = WebSocket::from_raw_socket(stream, Role::Server, None);
    loop {
        match ws_in.read() {
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => {}
        }
    }
    hub.remove(id);
    Ok(())
}
