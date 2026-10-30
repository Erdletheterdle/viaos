//! Byte streams multiplexed over one message-oriented connection.
//!
//! A device holds one outbound connection to the control plane. Everything
//! that crosses it -- an MCP session, later a shell or a TCP tunnel -- is a
//! stream opened on that connection. The carrier only has to move whole
//! binary messages in order, which a WebSocket does today and a QUIC stream
//! could do later without this crate changing.
//!
//! A frame is `[stream: u32 BE][kind: u8][payload]`:
//!
//! - `OPEN`  payload is the service name, e.g. `mcp`
//! - `DATA`  payload is bytes for the stream
//! - `CLOSE` no payload; the sender will write no more
//!
//! Streams opened by the control plane have odd ids and streams opened by the
//! device even ones, so the two sides never pick the same id.
//!
//! There is no per-stream flow control. A stream's reader that stops reading
//! stalls only that stream's buffer, never the connection: incoming data is
//! queued per stream, unbounded. That is the right trade for the traffic this
//! carries (request/response sessions and interactive shells), and the first
//! thing to revisit if bulk transfer moves onto it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::sync::mpsc;

const OPEN: u8 = 1;
const DATA: u8 = 2;
const CLOSE: u8 = 3;

/// Bytes buffered inside each stream's in-process pipe.
const PIPE: usize = 64 * 1024;
/// Largest payload in one frame.
const CHUNK: usize = 32 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    /// The control plane; opens odd-numbered streams.
    Server,
    /// The device; opens even-numbered streams.
    Device,
}

/// A stream the other side opened.
pub struct Incoming {
    pub service: String,
    pub stream: DuplexStream,
}

/// One end of a multiplexed connection.
///
/// Feed every message the carrier receives to [`Mux::receive`], and send
/// every message from the returned receiver out over the carrier.
#[derive(Clone)]
pub struct Mux {
    inner: Arc<Inner>,
}

struct Inner {
    out: mpsc::UnboundedSender<Vec<u8>>,
    accept: mpsc::UnboundedSender<Incoming>,
    streams: Mutex<Streams>,
}

struct Streams {
    next_id: u32,
    writers: HashMap<u32, mpsc::UnboundedSender<Vec<u8>>>,
}

impl Mux {
    /// Returns the mux, the messages to send over the carrier, and the
    /// streams the other side opens.
    pub fn new(
        side: Side,
    ) -> (Self, mpsc::UnboundedReceiver<Vec<u8>>, mpsc::UnboundedReceiver<Incoming>) {
        let (out, out_rx) = mpsc::unbounded_channel();
        let (accept, accept_rx) = mpsc::unbounded_channel();
        let next_id = match side {
            Side::Server => 1,
            Side::Device => 2,
        };
        let inner = Inner {
            out,
            accept,
            streams: Mutex::new(Streams { next_id, writers: HashMap::new() }),
        };
        (Self { inner: Arc::new(inner) }, out_rx, accept_rx)
    }

    /// Open a stream to `service` on the other side.
    pub fn open(&self, service: &str) -> DuplexStream {
        let id = {
            let mut s = self.inner.streams.lock().unwrap();
            let id = s.next_id;
            s.next_id = s.next_id.wrapping_add(2);
            id
        };
        let _ = self.inner.out.send(frame(id, OPEN, service.as_bytes()));
        self.attach(id)
    }

    /// Handle one message from the carrier.
    pub fn receive(&self, message: &[u8]) {
        let Some((id, kind, payload)) = parse(message) else {
            tracing::warn!(len = message.len(), "dropping malformed frame");
            return;
        };
        match kind {
            OPEN => {
                let service = String::from_utf8_lossy(payload).into_owned();
                let stream = self.attach(id);
                if self.inner.accept.send(Incoming { service, stream }).is_err() {
                    // Nobody accepts streams on this side; refuse it.
                    self.forget(id);
                    let _ = self.inner.out.send(frame(id, CLOSE, &[]));
                }
            }
            DATA => {
                let writers = &self.inner.streams.lock().unwrap().writers;
                if let Some(w) = writers.get(&id) {
                    let _ = w.send(payload.to_vec());
                }
            }
            CLOSE => self.forget(id),
            _ => tracing::warn!(kind, "dropping frame of unknown kind"),
        }
    }

    /// Number of open streams, for status reporting and tests.
    pub fn open_streams(&self) -> usize {
        self.inner.streams.lock().unwrap().writers.len()
    }

    fn forget(&self, id: u32) {
        // Dropping the writer ends the task feeding the pipe, which the
        // stream's reader then sees as end of file.
        self.inner.streams.lock().unwrap().writers.remove(&id);
    }

    /// Connect stream `id` to a fresh in-process pipe and return the user's
    /// end of it.
    fn attach(&self, id: u32) -> DuplexStream {
        let (user, ours) = tokio::io::duplex(PIPE);
        let (mut read_half, mut write_half) = tokio::io::split(ours);
        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        self.inner.streams.lock().unwrap().writers.insert(id, tx);

        // Carrier -> user.
        tokio::spawn(async move {
            while let Some(bytes) = rx.recv().await {
                if write_half.write_all(&bytes).await.is_err() {
                    break;
                }
            }
            let _ = write_half.shutdown().await;
        });

        // User -> carrier.
        let mux = self.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; CHUNK];
            loop {
                match read_half.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if mux.inner.out.send(frame(id, DATA, &buf[..n])).is_err() {
                            break;
                        }
                    }
                }
            }
            let _ = mux.inner.out.send(frame(id, CLOSE, &[]));
            mux.forget(id);
        });

        user
    }
}

fn frame(id: u32, kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(5 + payload.len());
    f.extend_from_slice(&id.to_be_bytes());
    f.push(kind);
    f.extend_from_slice(payload);
    f
}

fn parse(message: &[u8]) -> Option<(u32, u8, &[u8])> {
    let id = u32::from_be_bytes(message.get(..4)?.try_into().ok()?);
    Some((id, *message.get(4)?, &message[5..]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, BufReader};

    /// Two muxes joined back to back, standing in for a carrier.
    fn pair() -> (Mux, mpsc::UnboundedReceiver<Incoming>, Mux, mpsc::UnboundedReceiver<Incoming>) {
        let (server, mut s_out, s_accept) = Mux::new(Side::Server);
        let (device, mut d_out, d_accept) = Mux::new(Side::Device);
        let (s, d) = (server.clone(), device.clone());
        tokio::spawn(async move {
            while let Some(m) = s_out.recv().await {
                d.receive(&m);
            }
        });
        tokio::spawn(async move {
            while let Some(m) = d_out.recv().await {
                s.receive(&m);
            }
        });
        (server, s_accept, device, d_accept)
    }

    #[tokio::test]
    async fn line_round_trip() {
        let (server, _, _device, mut accept) = pair();
        let mut client = server.open("echo");

        tokio::spawn(async move {
            let Incoming { service, stream } = accept.recv().await.unwrap();
            assert_eq!(service, "echo");
            let (r, mut w) = tokio::io::split(stream);
            let mut lines = BufReader::new(r).lines();
            while let Some(line) = lines.next_line().await.unwrap() {
                w.write_all(format!("{line}!\n").as_bytes()).await.unwrap();
            }
        });

        client.write_all(b"hello\n").await.unwrap();
        let mut reader = BufReader::new(&mut client);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        assert_eq!(line, "hello!\n");
    }

    #[tokio::test]
    async fn close_reaches_the_other_side_as_eof() {
        let (server, _, _device, mut accept) = pair();
        let mut client = server.open("sink");
        client.write_all(b"abc").await.unwrap();
        client.shutdown().await.unwrap();

        let Incoming { mut stream, .. } = accept.recv().await.unwrap();
        let mut got = Vec::new();
        stream.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, b"abc");
    }

    #[tokio::test]
    async fn ids_do_not_collide() {
        let (server, mut s_accept, device, mut d_accept) = pair();
        let _a = server.open("x");
        let _b = device.open("y");
        assert_eq!(d_accept.recv().await.unwrap().service, "x");
        assert_eq!(s_accept.recv().await.unwrap().service, "y");
    }

    #[test]
    fn malformed_frames_are_rejected() {
        assert!(parse(&[0, 0, 1]).is_none());
        assert_eq!(parse(&[0, 0, 0, 7, DATA, 9]), Some((7, DATA, &[9][..])));
    }
}
