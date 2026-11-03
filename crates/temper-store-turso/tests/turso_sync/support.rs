//! Helpers shared by the Turso Sync tests: a local sync server, a machine's
//! data folder, and writes to count.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};
use temper_store_turso::TursoEventStore;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

pub const TOKEN: &str = "local-sync-server-ignores-tokens";

pub struct SyncServer {
    child: Child,
    pub url: String,
    _dir: tempfile::TempDir,
}

impl SyncServer {
    pub fn start() -> Option<Self> {
        let bin = temper_test_resource::env_var("TEMPER_TEST_TURSODB")?;
        // The store's and the sync engine's logs, and the server's below, show
        // in the output of a test that fails or times out.
        let _ = tracing_subscriber::fmt()
            .with_env_filter("temper_store_turso=debug,turso_sync_engine=info")
            .with_writer(std::io::stderr)
            .try_init();
        let dir = tempfile::tempdir().expect("server dir");
        let port = TcpListener::bind("127.0.0.1:0")
            .and_then(|listener| listener.local_addr())
            .expect("free port")
            .port();
        let child = Command::new(bin)
            .arg(dir.path().join("remote.db"))
            .args(["--sync-server", &format!("127.0.0.1:{port}")])
            .env("RUST_LOG", "info")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start tursodb --sync-server");
        let server = Self {
            child,
            url: format!("http://127.0.0.1:{port}"),
            _dir: dir,
        };
        // Ready once it answers a statement, not merely once its port is bound.
        let started = Instant::now();
        while server
            .post("SELECT 1")
            .is_none_or(|answer| !answer.starts_with("HTTP/1.1 200"))
        {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "sync server did not start"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        Some(server)
    }

    fn post(&self, sql: &str) -> Option<String> {
        let addr = self.url.trim_start_matches("http://");
        let body = serde_json::json!({
            "requests": [{ "type": "execute", "stmt": { "sql": sql } }]
        })
        .to_string();
        let mut stream = TcpStream::connect(addr).ok()?;
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .ok()?;
        write!(
            stream,
            "POST /v2/pipeline HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .ok()?;
        let mut answer = String::new();
        stream.read_to_string(&mut answer).ok()?;
        Some(answer)
    }

    pub async fn open(&self, path: &Path) -> TursoEventStore {
        TursoEventStore::new_synced(path.to_str().expect("utf-8 path"), &self.url, TOKEN)
            .await
            .expect("open synced store")
    }
}

impl SyncServer {
    /// Run one statement on the remote database directly.
    pub fn remote_sql(&self, sql: &str) {
        let answer = self.post(sql).expect("sync server answers");
        assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
        assert!(!answer.contains("\"type\":\"error\""), "{answer}");
    }
}

impl Drop for SyncServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn machine() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("machine dir");
    let path = dir.path().join("app.db");
    (dir, path)
}

pub fn event(n: u64) -> PersistenceEnvelope {
    PersistenceEnvelope {
        sequence_nr: 0,
        event_type: "Written".to_string(),
        payload: serde_json::json!({ "n": n, "body": "x".repeat(1500) }),
        metadata: EventMetadata {
            event_id: uuid::Uuid::new_v4(),
            causation_id: uuid::Uuid::new_v4(),
            correlation_id: uuid::Uuid::new_v4(),
            timestamp: chrono::Utc::now(),
            actor_id: "turso-sync-test".to_string(),
        },
    }
}

pub async fn write(store: &TursoEventStore, entity: &str, after: u64) {
    store
        .append(
            &format!("default:Note:{entity}"),
            after,
            &[event(after + 1)],
        )
        .await
        .expect("append");
}

pub async fn count(store: &TursoEventStore, entity: &str) -> usize {
    store
        .read_events(&format!("default:Note:{entity}"), 0)
        .await
        .expect("read events")
        .len()
}

pub async fn push(store: &TursoEventStore) {
    store
        .sync_copy()
        .expect("synced store has a copy")
        .push_committed()
        .await
        .expect("push");
}

/// Confirmed change log rows for the long-log tests: more than half the
/// Factory's 532,000 changes.
pub const BIG_LOG: i64 = 300_000;

/// Open a store at `path` whose change log holds `rows` changes the remote
/// has confirmed, below one pushed write.
///
/// A push carries only changes numbered above the remote's record of this
/// file's last push, so every row at or below it is the confirmed part of
/// the log. Pushing 300,000 changes for real is one request of one
/// statement per change, far past a push's 10 s limit, so the confirmed
/// part is written into the log directly, numbered below the first real
/// change. Most rows hold a 1 KB image and every hundredth holds 30 KB,
/// the size of the Factory's catalog images, which spill onto overflow
/// pages. The images are zeros: turso stores them uncompressed, so they
/// take the same pages as real ones, and `randomblob` made building the
/// log several times slower on a CI runner. The pinned release `tursodb`
/// builds the fixture with the same engine and SQL, avoiding minutes of
/// debug-build inserts before the behavior under test starts. The store
/// is closed first: the CLI must be the only writer of this database.
/// CI's disk is warm: the cold rate of 34.2 s per 10,000 rows is roll3's
/// production measurement on a Tensorlake drive, not this test's.
pub async fn confirmed_log(
    server: &SyncServer,
    path: &Path,
    rows: i64,
) -> (TursoEventStore, temper_store_turso::SyncCopy) {
    let store = server.open(path).await;
    let copy = store.sync_copy().expect("copy").clone();
    write(&store, "pushed", 0).await;
    push(&store).await;
    let before = copy.change_log_rows().await.expect("rows");
    let newest = copy.last_change().await.expect("newest change");
    drop(copy);
    drop(store);
    let bin = temper_test_resource::env_var("TEMPER_TEST_TURSODB")
        .expect("the running sync server's binary");
    let output = tokio::process::Command::new(bin)
        .arg("--quiet")
        .arg(path)
        .arg(format!(
            "INSERT INTO turso_cdc \
             (change_id, change_time, change_type, table_name, id, before, after, updates) \
             SELECT value - {rows}, 0, 1, 'events', value, NULL, \
             CASE WHEN value % 100 = 0 THEN zeroblob(30000) ELSE zeroblob(1000) END, NULL \
             FROM generate_series(1, {rows})"
        ))
        .output()
        .await
        .expect("fill the change log");
    assert!(
        output.status.success(),
        "fill the change log: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let store = server.open(path).await;
    let copy = store.sync_copy().expect("copy").clone();
    copy.checkpoint_if_wal_over(0).await.expect("checkpoint");
    assert_eq!(copy.change_log_rows().await.expect("rows"), before + rows);
    assert!(
        copy.last_change().await.expect("newest change") >= newest,
        "reopening the fixture must not rewind change numbers"
    );
    (store, copy)
}

pub fn copy_files(from: &Path, to: &Path) {
    for entry in std::fs::read_dir(from).expect("read dir") {
        let entry = entry.expect("entry");
        if entry.file_type().expect("type").is_file() {
            std::fs::copy(entry.path(), to.join(entry.file_name())).expect("copy");
        }
    }
}

pub fn remove_files(dir: &Path) {
    for entry in std::fs::read_dir(dir).expect("read dir") {
        let entry = entry.expect("entry");
        if entry.file_type().expect("type").is_file() {
            std::fs::remove_file(entry.path()).expect("remove");
        }
    }
}

/// Forwards to the sync server until closed, then swallows every byte, as a
/// remote that stops answering.
pub async fn valve(upstream: &str) -> (String, std::sync::Arc<std::sync::atomic::AtomicBool>) {
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let open = std::sync::Arc::new(AtomicBool::new(true));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    let upstream = upstream.trim_start_matches("http://").to_string();
    let flag = open.clone();
    tokio::spawn(async move {
        while let Ok((client, _)) = listener.accept().await {
            let Ok(server) = tokio::net::TcpStream::connect(&upstream).await else {
                continue;
            };
            let (mut client_read, mut client_write) = client.into_split();
            let (mut server_read, mut server_write) = server.into_split();
            let up = flag.clone();
            tokio::spawn(async move {
                let mut buf = vec![0; 64 * 1024];
                while let Ok(n) = client_read.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    if up.load(Ordering::Acquire)
                        && server_write.write_all(&buf[..n]).await.is_err()
                    {
                        break;
                    }
                }
            });
            tokio::spawn(async move {
                let mut buf = vec![0; 64 * 1024];
                while let Ok(n) = server_read.read(&mut buf).await {
                    if n == 0 || client_write.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    (url, open)
}

/// Forwards to the sync server while on; while off, closes every new
/// connection at once, as a remote that cannot be reached.
pub async fn switch(upstream: &str) -> (String, std::sync::Arc<std::sync::atomic::AtomicBool>) {
    use std::sync::atomic::Ordering;
    let on = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    let upstream = upstream.trim_start_matches("http://").to_string();
    let flag = on.clone();
    tokio::spawn(async move {
        while let Ok((mut client, _)) = listener.accept().await {
            if !flag.load(Ordering::Acquire) {
                continue;
            }
            let upstream = upstream.clone();
            tokio::spawn(async move {
                if let Ok(mut server) = tokio::net::TcpStream::connect(&upstream).await {
                    let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
                }
            });
        }
    });
    (url, on)
}

/// Forwards to the sync server, counting the HTTP requests sent through it.
pub async fn counting(upstream: &str) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let requests = std::sync::Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    let upstream = upstream.trim_start_matches("http://").to_string();
    let counted = requests.clone();
    tokio::spawn(async move {
        while let Ok((client, _)) = listener.accept().await {
            let Ok(server) = tokio::net::TcpStream::connect(&upstream).await else {
                continue;
            };
            let (mut client_read, mut client_write) = client.into_split();
            let (mut server_read, mut server_write) = server.into_split();
            let counted = counted.clone();
            tokio::spawn(async move {
                // Each request line ends with its version; the last bytes of
                // one read are kept so a line split across two reads counts.
                const START: &[u8] = b" HTTP/1.1\r\n";
                let mut seen: Vec<u8> = Vec::new();
                let mut buf = vec![0; 64 * 1024];
                while let Ok(n) = client_read.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    seen.extend_from_slice(&buf[..n]);
                    let found = seen.windows(START.len()).filter(|w| *w == START).count();
                    counted.fetch_add(found, Ordering::AcqRel);
                    let keep = seen.len().min(START.len() - 1);
                    seen.drain(..seen.len() - keep);
                    if server_write.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
            tokio::spawn(async move {
                let mut buf = vec![0; 64 * 1024];
                while let Ok(n) = server_read.read(&mut buf).await {
                    if n == 0 || client_write.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    (url, requests)
}

/// One logged line: the span it was logged in, its message and its fields.
#[derive(Clone, Debug)]
pub struct Line {
    pub span: Option<String>,
    pub target: String,
    pub message: String,
    pub fields: BTreeMap<String, String>,
}

/// Every line logged on this thread while it listens.
#[derive(Clone, Default)]
pub struct Lines(Arc<Mutex<Vec<Line>>>);

impl Lines {
    pub fn find(&self, matches: impl Fn(&Line) -> bool) -> Option<Line> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .find(|line| matches(line))
            .cloned()
    }
}

impl<S> tracing_subscriber::Layer<S> for Lines
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        #[derive(Default)]
        struct Fields(BTreeMap<String, String>);
        impl tracing::field::Visit for Fields {
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                self.0.insert(field.name().to_string(), value.to_string());
            }
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0
                    .insert(field.name().to_string(), format!("{value:?}"));
            }
        }
        let mut fields = Fields::default();
        event.record(&mut fields);
        let message = fields.0.remove("message").unwrap_or_default();
        self.0.lock().unwrap().push(Line {
            span: ctx.event_span(event).map(|span| span.name().to_string()),
            target: event.metadata().target().to_string(),
            message,
            fields: fields.0,
        });
    }
}
