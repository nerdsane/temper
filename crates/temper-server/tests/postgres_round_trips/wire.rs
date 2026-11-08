//! Postgres wire frames and the proxy that counts their round trips.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Counts the client messages that wait for an answer from the server, and
/// keeps each one's description: its type and the statements it runs.
#[derive(Clone, Default)]
pub(super) struct RoundTrips(Arc<AtomicU64>, Arc<Mutex<Vec<String>>>);

impl RoundTrips {
    /// Number of messages counted so far.
    pub(super) fn now(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }

    /// The counted messages from `from` on, one line each.
    pub(super) fn since(&self, from: u64) -> Vec<String> {
        self.1.lock().unwrap()[from as usize..].to_vec()
    }
}

/// Forward every connection on a local port to `upstream`, counting the
/// round trips the client starts.
pub(super) async fn proxy(upstream: (String, u16)) -> (u16, RoundTrips) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let trips = RoundTrips::default();
    let counted = trips.clone();
    tokio::spawn(async move {
        loop {
            let Ok((client, _)) = listener.accept().await else {
                return;
            };
            let server = TcpStream::connect((upstream.0.as_str(), upstream.1))
                .await
                .unwrap();
            tokio::spawn(relay(client, server, counted.clone()));
        }
    });
    (port, trips)
}

async fn relay(client: TcpStream, server: TcpStream, trips: RoundTrips) {
    let (mut client_read, mut client_write) = client.into_split();
    let (mut server_read, mut server_write) = server.into_split();
    tokio::spawn(async move {
        let _ = tokio::io::copy(&mut server_read, &mut client_write).await;
    });
    let mut frames = Frames::default();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        let read = match client_read.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(read) => read,
        };
        // Count before forwarding, so the answer cannot arrive first.
        let waits = frames.push(&chunk[..read]);
        if !waits.is_empty() {
            let mut log = trips.1.lock().unwrap();
            trips.0.fetch_add(waits.len() as u64, Ordering::SeqCst);
            log.extend(waits);
        }
        if server_write.write_all(&chunk[..read]).await.is_err() {
            return;
        }
    }
}

/// The frontend message stream: untyped messages (`[len][body]`) until the
/// startup message, then typed ones (`[type][len][body]`).
#[derive(Default)]
struct Frames {
    pending: Vec<u8>,
    started: bool,
    /// Each prepared statement's text, by name, as its Parse gave it.
    prepared: BTreeMap<String, String>,
    /// The statements of the extended-protocol batch a Sync will end.
    batch: Vec<String>,
}

/// The first `limit` characters of `text` on one line.
fn excerpt(text: &str, limit: usize) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(limit)
        .collect()
}

/// The NUL-terminated strings at the start of `body`.
fn cstrings(body: &[u8], count: usize) -> Vec<String> {
    body.split(|byte| *byte == 0)
        .take(count)
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect()
}

const SSL_REQUEST: u32 = 80_877_103;
const GSS_REQUEST: u32 = 80_877_104;

impl Frames {
    /// Take in `bytes` and describe each complete message among them that
    /// waits for an answer.
    fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.pending.extend_from_slice(bytes);
        let mut waits = Vec::new();
        loop {
            let header = if self.started { 5 } else { 8 };
            if self.pending.len() < header {
                return waits;
            }
            let at = usize::from(self.started);
            let length = u32::from_be_bytes(self.pending[at..at + 4].try_into().unwrap()) as usize;
            assert!(length >= 4, "a frontend message length counts itself");
            if self.pending.len() < at + length {
                return waits;
            }
            if self.started {
                let body = &self.pending[5..at + length];
                match self.pending[0] {
                    b'Q' => waits.push(format!("Q {}", excerpt(&cstrings(body, 1)[0], 120))),
                    b'P' => {
                        let parts = cstrings(body, 2);
                        let text = excerpt(&parts[1], 120);
                        self.prepared.insert(parts[0].clone(), text.clone());
                        self.batch.push(format!("Parse {:?}: {text}", parts[0]));
                    }
                    b'B' => {
                        let name = cstrings(body, 2).remove(1);
                        let text = self.prepared.get(&name).cloned().unwrap_or_default();
                        self.batch.push(format!("Bind {name:?}: {text}"));
                    }
                    b'S' => {
                        let batch = std::mem::take(&mut self.batch);
                        waits.push(if batch.is_empty() {
                            "S (bare Sync: the pool's ping)".to_string()
                        } else {
                            format!("S [{}]", batch.join(" ; "))
                        });
                    }
                    _ => {}
                }
            } else {
                let code = u32::from_be_bytes(self.pending[4..8].try_into().unwrap());
                self.started = !matches!(code, SSL_REQUEST | GSS_REQUEST);
            }
            self.pending.drain(..at + length);
        }
    }
}
