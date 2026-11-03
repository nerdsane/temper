//! Every push the remote answers is counted by its answer, so a remote that
//! refuses every write shows in monitoring while the local file still takes
//! them (staging's blocked Turso org, 3 Nov 2026).
//!
//! It sets the process's meter provider, which the counter binds to the
//! first time anything records it, so it needs a process of its own: CI's
//! nextest runs every test in one.

use std::collections::BTreeMap;

use opentelemetry::KeyValue;
use opentelemetry_sdk::metrics::data::Sum;
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, SdkMeterProvider};
use temper_store_turso::{CopyState, TursoEventStore};

use super::support::*;

/// The pushes counted so far, by `outcome` and `code`.
fn pushes(
    provider: &SdkMeterProvider,
    exporter: &InMemoryMetricExporter,
) -> BTreeMap<(String, String), u64> {
    provider.force_flush().expect("flush metrics");
    let value_of = |attributes: &[KeyValue], key: &str| {
        attributes
            .iter()
            .find(|kv| kv.key.as_str() == key)
            .map(|kv| kv.value.as_str().to_string())
            .unwrap_or_default()
    };
    let mut counted = BTreeMap::new();
    // Cumulative sums, oldest export first: the last one read is the latest.
    for metrics in exporter.get_finished_metrics().expect("metrics") {
        for scope in &metrics.scope_metrics {
            for metric in scope
                .metrics
                .iter()
                .filter(|metric| metric.name == "temper_turso_sync_pushes")
            {
                let sum = metric
                    .data
                    .as_any()
                    .downcast_ref::<Sum<u64>>()
                    .expect("a counter");
                for point in &sum.data_points {
                    let key = (
                        value_of(&point.attributes, "outcome"),
                        value_of(&point.attributes, "code"),
                    );
                    counted.insert(key, point.value);
                }
            }
        }
    }
    counted
}

fn counted_as(counted: &BTreeMap<(String, String), u64>, outcome: &str, code: &str) -> u64 {
    counted
        .get(&(outcome.to_string(), code.to_string()))
        .copied()
        .unwrap_or(0)
}

#[tokio::test]
async fn a_push_the_remote_refuses_counts_refused_with_its_code_and_one_it_takes_confirmed() {
    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_periodic_exporter(exporter.clone())
        .build();
    opentelemetry::global::set_meter_provider(provider.clone());
    let Some(server) = SyncServer::start() else {
        return;
    };
    let (url, refusing) = refuser(&server.url).await;
    let (_dir, path) = machine();
    let store = TursoEventStore::new_synced(path.to_str().expect("utf-8"), &url, TOKEN)
        .await
        .expect("open through the refuser");
    let copy = store.sync_copy().expect("copy").clone();

    // The org's writes are blocked: the local file takes the write, the
    // remote refuses its push.
    refusing.store(true, std::sync::atomic::Ordering::Release);
    write(&store, "b1", 0).await;
    let error = copy
        .push_committed()
        .await
        .expect_err("the remote refused the push");
    assert!(error.to_string().contains("BLOCKED"), "{error}");
    assert_eq!(
        copy.state(),
        CopyState::Copying,
        "a refusal that committed nothing is retried, not a stop"
    );
    let counted = pushes(&provider, &exporter);
    assert_eq!(counted_as(&counted, "refused", "BLOCKED"), 1, "{counted:?}");
    assert_eq!(counted_as(&counted, "confirmed", ""), 0, "{counted:?}");

    // Unblocked: the same change goes, and the remote confirms it.
    refusing.store(false, std::sync::atomic::Ordering::Release);
    copy.push_committed()
        .await
        .expect("the remote takes the push");
    let counted = pushes(&provider, &exporter);
    assert_eq!(counted_as(&counted, "confirmed", ""), 1, "{counted:?}");
    assert_eq!(counted_as(&counted, "refused", "BLOCKED"), 1, "{counted:?}");
    assert_eq!(counted_as(&counted, "failed", ""), 0, "{counted:?}");
    drop(copy);
    drop(store);

    // After a restart the first push finds everything already on the
    // remote: it sends nothing, so nothing is confirmed.
    let store = TursoEventStore::new_synced(path.to_str().expect("utf-8"), &url, TOKEN)
        .await
        .expect("reopen through the refuser");
    push(&store).await;
    let counted = pushes(&provider, &exporter);
    assert_eq!(
        counted_as(&counted, "confirmed", ""),
        1,
        "a push that sent nothing was counted confirmed: {counted:?}"
    );

    let (_b_dir, b_path) = machine();
    let b = server.open(&b_path).await;
    assert_eq!(count(&b, "b1").await, 1, "the change reached the remote");
}

/// What [`refuser`] answers a push with while refusing: a Hrana stream
/// error with the message and code staging's blocked org gave every write on
/// 3 Nov 2026.
const BLOCKED_ANSWER: &str = r#"{"baton":null,"base_url":null,"results":[{"type":"error","error":{"message":"SQL write operations are forbidden (writes are blocked, do you need to upgrade your plan?)","code":"BLOCKED"}}]}"#;

/// Forwards to the sync server, except that while refusing it answers every
/// push (a request that opens a write transaction) with [`BLOCKED_ANSWER`],
/// as Turso does for an org whose writes are blocked. Reads still reach the
/// server, as they do on a blocked org. Each request gets its own
/// connection (`Connection: close` both ways), so every one is looked at.
async fn refuser(upstream: &str) -> (String, std::sync::Arc<std::sync::atomic::AtomicBool>) {
    let refusing = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    let upstream = upstream.trim_start_matches("http://").to_string();
    let flag = refusing.clone();
    tokio::spawn(async move {
        while let Ok((client, _)) = listener.accept().await {
            let upstream = upstream.clone();
            let flag = flag.clone();
            tokio::spawn(async move {
                let _ = answer_one(client, &upstream, &flag).await;
            });
        }
    });
    (url, refusing)
}

async fn answer_one(
    mut client: tokio::net::TcpStream,
    upstream: &str,
    refusing: &std::sync::atomic::AtomicBool,
) -> std::io::Result<()> {
    use std::sync::atomic::Ordering;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let Some((head, body)) = read_request(&mut client).await? else {
        return Ok(());
    };
    let opens_a_write = String::from_utf8_lossy(&body).contains("BEGIN IMMEDIATE");
    if refusing.load(Ordering::Acquire) && opens_a_write {
        let answer = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{BLOCKED_ANSWER}",
            BLOCKED_ANSWER.len()
        );
        client.write_all(answer.as_bytes()).await?;
        return client.shutdown().await;
    }
    let mut server = tokio::net::TcpStream::connect(upstream).await?;
    server.write_all(closing(&head).as_bytes()).await?;
    server.write_all(&body).await?;
    let mut answer = Vec::new();
    server.read_to_end(&mut answer).await?;
    let split = find(&answer, b"\r\n\r\n").map_or(answer.len(), |at| at + 4);
    let answer_head = String::from_utf8_lossy(&answer[..split]).into_owned();
    client.write_all(closing(&answer_head).as_bytes()).await?;
    client.write_all(&answer[split..]).await?;
    client.shutdown().await
}

/// One HTTP request's head and its `Content-Length` body; `None` when the
/// client closed before sending one.
async fn read_request(
    client: &mut tokio::net::TcpStream,
) -> std::io::Result<Option<(String, Vec<u8>)>> {
    use tokio::io::AsyncReadExt;
    let mut seen = Vec::new();
    let mut buf = vec![0; 64 * 1024];
    let split = loop {
        if let Some(at) = find(&seen, b"\r\n\r\n") {
            break at + 4;
        }
        let n = client.read(&mut buf).await?;
        if n == 0 {
            return Ok(None);
        }
        seen.extend_from_slice(&buf[..n]);
    };
    let head = String::from_utf8_lossy(&seen[..split]).into_owned();
    let length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    let mut body = seen[split..].to_vec();
    while body.len() < length {
        let n = client.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&buf[..n]);
    }
    Ok(Some((head, body)))
}

/// `head` with its `Connection` header replaced by `Connection: close`.
fn closing(head: &str) -> String {
    let mut lines: Vec<&str> = head
        .trim_end_matches("\r\n")
        .split("\r\n")
        .filter(|line| {
            !line
                .split_once(':')
                .is_some_and(|(name, _)| name.trim().eq_ignore_ascii_case("connection"))
        })
        .collect();
    lines.push("Connection: close");
    format!("{}\r\n\r\n", lines.join("\r\n"))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
