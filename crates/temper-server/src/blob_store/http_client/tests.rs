use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;

use super::super::BlobStore;
use super::*;

// RSS comes from procfs, which only Linux has.
#[cfg(target_os = "linux")]
fn rss_bytes() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").expect("status");
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kb: u64 = rest.split_whitespace().next().unwrap().parse().unwrap();
            return kb * 1024;
        }
    }
    panic!("VmRSS missing");
}

struct Peer {
    url_base: String,
    accepts: Arc<AtomicUsize>,
    requests: Arc<AtomicUsize>,
}

async fn serve(body_for: impl Fn(usize) -> Vec<u8> + Send + Sync + 'static, stall: bool) -> Peer {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let accepts = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(AtomicUsize::new(0));
    let accepts_task = Arc::clone(&accepts);
    let requests_task = Arc::clone(&requests);
    let body_for = Arc::new(body_for);
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            accepts_task.fetch_add(1, Ordering::SeqCst);
            let requests_task = Arc::clone(&requests_task);
            let body_for = Arc::clone(&body_for);
            tokio::spawn(async move {
                if stall {
                    tokio::time::sleep(Duration::from_secs(30)).await; // determinism-ok: test peer that never answers
                    return;
                }
                let mut buf = Vec::new();
                let mut tmp = [0u8; 2048];
                loop {
                    let n = match socket.read(&mut tmp).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    buf.extend_from_slice(&tmp[..n]);
                    while let Some(end) = header_end(&buf) {
                        let n = requests_task.fetch_add(1, Ordering::SeqCst);
                        let body = (*body_for)(n);
                        let header = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
                            body.len()
                        );
                        if socket.write_all(header.as_bytes()).await.is_err() {
                            return;
                        }
                        if socket.write_all(&body).await.is_err() {
                            return;
                        }
                        buf.drain(..end);
                    }
                }
            });
        }
    });
    Peer {
        url_base: format!("http://{addr}"),
        accepts,
        requests,
    }
}

fn header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
}

fn store(peer: &Peer) -> BlobStore {
    BlobStore::s3(&peer.url_base, "bucket", None, None, "/tmp", None)
}

#[test]
fn cache_evicts_the_oldest_body_inside_the_byte_budget() {
    let mut cache = BlobGetCache {
        bodies: BTreeMap::new(),
        by_age: BTreeMap::new(),
        next_seq: 0,
        total_bytes: 0,
        max_entry: 4,
        max_total: 8,
    };
    cache.insert("a".to_string(), b"aaaa");
    cache.insert("b".to_string(), b"bbbb");
    cache.insert("c".to_string(), b"cccc");
    assert!(cache.get("a").is_none(), "oldest entry must leave");
    assert_eq!(cache.get("b").as_deref(), Some(b"bbbb".as_slice()));
    assert_eq!(cache.total_bytes, 8);
    cache.insert("wide".to_string(), b"too-big");
    assert!(cache.get("wide").is_none());
    assert_eq!(cache.get("c").as_deref(), Some(b"cccc".as_slice()));
}

#[tokio::test]
async fn sequential_gets_reuse_one_connection_across_dropped_stores() {
    let peer = serve(|_| b"one".to_vec(), false).await;
    let first = store(&peer);
    let left = first.get("a").await.expect("first get");
    drop(first);
    let second = store(&peer);
    let right = second.get("b").await.expect("second get");
    assert_eq!(left.as_deref(), Some(b"one".as_slice()));
    assert_eq!(right.as_deref(), Some(b"one".as_slice()));
    assert_eq!(
        peer.requests.load(Ordering::SeqCst),
        2,
        "distinct keys must both reach the origin"
    );
    assert_eq!(
        peer.accepts.load(Ordering::SeqCst),
        1,
        "dropping the store must not drop the shared connection pool"
    );
}

#[tokio::test]
async fn a_second_get_of_the_same_object_does_not_hit_the_origin() {
    let peer = serve(|_| b"cached-body".to_vec(), false).await;
    let store = store(&peer);
    let first = store.get("same").await.expect("first");
    let second = store.get("same").await.expect("second");
    assert_eq!(first, second);
    assert_eq!(peer.requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn bodies_over_the_entry_cap_are_not_cached() {
    let cap = BLOB_GET_CACHE_MAX_ENTRY_BYTES + 1;
    let peer = serve(move |_| vec![9u8; cap], false).await;
    let store = store(&peer);
    let first = store.get("wide").await.expect("first").expect("body");
    let second = store.get("wide").await.expect("second").expect("body");
    assert_eq!(first.len(), cap);
    assert_eq!(second.len(), cap);
    assert_eq!(peer.requests.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_peer_that_accepts_and_never_answers_fails_inside_the_header_budget() {
    let peer = serve(|_| Vec::new(), true).await;
    let store = store(&peer);
    let started = Instant::now(); // determinism-ok: asserts the hung GET fails quickly
    let error = store.get("hung").await.expect_err("hung get must fail");
    let elapsed = started.elapsed();
    assert!(
        error.contains("timed out") || error.contains("failed"),
        "unexpected error: {error}"
    );
    assert!(
        elapsed < Duration::from_secs(4),
        "hung GET waited {elapsed:?}; the five-minute outer deadline must not be the only stop"
    );
}

#[tokio::test]
async fn a_reset_connection_is_retried_once() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let accepts = Arc::new(AtomicUsize::new(0));
    let accepts_task = Arc::clone(&accepts);
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let n = accepts_task.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                drop(socket);
                continue;
            }
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf).await;
            let body = b"retried";
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = socket.write_all(header.as_bytes()).await;
            let _ = socket.write_all(body).await;
        }
    });
    let store = BlobStore::s3(format!("http://{addr}"), "bucket", None, None, "/tmp", None);
    let bytes = store.get("obj").await.expect("retry").expect("body");
    assert_eq!(bytes, b"retried");
    assert!(accepts.load(Ordering::SeqCst) >= 2);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn the_cache_budget_bounds_retained_rss_across_repeated_fills() {
    let before = rss_bytes();
    let mut peers = Vec::new();
    for index in 0..20 {
        let peer = serve(move |_| vec![index as u8; 64 * 1024], false).await;
        let store = store(&peer);
        let key = format!("k{index}");
        let bytes = store.get(&key).await.expect("get").expect("body");
        assert_eq!(bytes.len(), 64 * 1024);
        peers.push(peer);
    }
    let filled = rss_bytes();
    for (index, peer) in peers.iter().enumerate() {
        let store = store(peer);
        let again = store
            .get(&format!("k{index}"))
            .await
            .expect("cached")
            .expect("body");
        assert_eq!(again.len(), 64 * 1024);
        assert_eq!(
            peer.requests.load(Ordering::SeqCst),
            1,
            "a 64KiB body must stay cached"
        );
    }
    let after = rss_bytes();
    let grew = after.saturating_sub(filled);
    assert!(
        grew < 8 * 1024 * 1024,
        "second fill grew RSS by {grew} (before={before} filled={filled} after={after})"
    );
}
