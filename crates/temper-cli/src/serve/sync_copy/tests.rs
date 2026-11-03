use super::*;

#[test]
fn only_reads_of_temper_routes_skip_the_push() {
    assert!(reads_only(&Method::GET, "/tdata/Notes('a')"));
    assert!(reads_only(&Method::GET, "/tdata"));
    assert!(reads_only(&Method::HEAD, "/healthz"));
    assert!(reads_only(&Method::GET, "/static/temper-client.js"));
    assert!(!reads_only(&Method::POST, "/tdata/Notes('a')/Ns.Write"));
    assert!(!reads_only(&Method::GET, "/webhooks/default/callback"));
    assert!(!reads_only(&Method::GET, "/oauth/callback"));
    assert!(!reads_only(&Method::GET, "/static/app-page"));
    assert!(!reads_only(&Method::GET, "/observer"));
    assert!(!reads_only(&Method::GET, "/genesisx"));
}

#[tokio::test]
async fn a_not_confirmed_answer_names_the_key_its_write_ran_with() {
    let key = HeaderValue::from_static("write-key-1");
    let answer = not_confirmed(Some(&key));
    assert_eq!(answer.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(answer.headers().get(IDEMPOTENCY_KEY), Some(&key));
    let body = axum::body::to_bytes(answer.into_body(), 64 * 1024)
        .await
        .expect("body");
    let body: serde_json::Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(body["error"]["code"], "NotConfirmed");
    assert_eq!(body["error"]["idempotencyKey"], "write-key-1");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("Send it again with its Idempotency-Key"),
        "{message}"
    );

    // A request that came without one (an app's own route) still gets
    // the answer, without a key.
    let unkeyed = not_confirmed(None);
    assert!(unkeyed.headers().get(IDEMPOTENCY_KEY).is_none());
}

/// A span's name, its parent's name, and the fields given a value.
#[derive(Debug)]
struct SpanSeen {
    name: String,
    parent: Option<String>,
    fields: std::collections::BTreeSet<String>,
}

/// Every span made, in order, and the fields recorded on it later.
#[derive(Clone, Default)]
struct Spans(std::sync::Arc<std::sync::Mutex<Vec<SpanSeen>>>);

/// Collects the names of the fields a span is given.
struct FieldNames<'a>(&'a mut std::collections::BTreeSet<String>);

impl tracing::field::Visit for FieldNames<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, _: &dyn std::fmt::Debug) {
        self.0.insert(field.name().to_string());
    }
}

impl<S> tracing_subscriber::Layer<S> for Spans
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let span = ctx.span(id).expect("a new span is registered");
        let mut fields = std::collections::BTreeSet::new();
        attrs.record(&mut FieldNames(&mut fields));
        let mut seen = self.0.lock().expect("spans");
        span.extensions_mut().insert(seen.len());
        seen.push(SpanSeen {
            name: span.name().to_string(),
            parent: span.parent().map(|parent| parent.name().to_string()),
            fields,
        });
    }

    fn on_record(
        &self,
        id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let span = ctx.span(id).expect("a recorded span is registered");
        let Some(&at) = span.extensions().get::<usize>() else {
            return;
        };
        let mut seen = self.0.lock().expect("spans");
        values.record(&mut FieldNames(&mut seen[at].fields));
    }
}

/// Turso's local sync server (`tursodb --sync-server`), the remote the
/// store's turso_sync tests use.
struct SyncServer {
    child: std::process::Child,
    url: String,
    _dir: tempfile::TempDir,
}

impl SyncServer {
    async fn start() -> Option<Self> {
        let bin = temper_test_resource::env_var("TEMPER_TEST_TURSODB")?;
        let dir = tempfile::tempdir().expect("server dir");
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|listener| listener.local_addr())
            .expect("free port")
            .port();
        let child = std::process::Command::new(bin)
            .arg(dir.path().join("remote.db"))
            .args(["--sync-server", &format!("127.0.0.1:{port}")])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("start tursodb --sync-server");
        let server = Self {
            child,
            url: format!("http://127.0.0.1:{port}"),
            _dir: dir,
        };
        // Ready once it answers a statement, not merely once its port is bound.
        let started = Instant::now();
        let client = reqwest::Client::new();
        let select = serde_json::json!({
            "requests": [{ "type": "execute", "stmt": { "sql": "SELECT 1" } }]
        });
        loop {
            let answer = client
                .post(format!("{}/v2/pipeline", server.url))
                .json(&select)
                .send()
                .await;
            if answer.is_ok_and(|answer| answer.status().is_success()) {
                return Some(server);
            }
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "sync server did not start"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

impl Drop for SyncServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A write's push is part of the write's trace: it runs under the
/// request's span, so a slow answer shows its push, and says how long it
/// queued behind another push.
#[tokio::test]
async fn a_writes_push_is_inside_its_request_span() {
    use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};
    use tracing_subscriber::layer::SubscriberExt as _;

    let Some(server) = SyncServer::start().await else {
        return;
    };
    let dir = tempfile::tempdir().expect("data dir");
    let local = format!("file:{}", dir.path().join("app.db").display());
    let store = TursoEventStore::new_synced(&local, &server.url, "the-local-server-ignores-it")
        .await
        .expect("open the synced store");
    let writer = store.clone();
    let routes = Router::new().route(
        "/write",
        axum::routing::post(move || async move {
            let event = PersistenceEnvelope {
                sequence_nr: 0,
                event_type: "Written".to_string(),
                payload: serde_json::json!({ "n": 1 }),
                metadata: EventMetadata {
                    event_id: uuid::Uuid::new_v4(),
                    causation_id: uuid::Uuid::new_v4(),
                    correlation_id: uuid::Uuid::new_v4(),
                    timestamp: chrono::Utc::now(),
                    actor_id: "serve-test".to_string(),
                },
            };
            writer
                .append("default:Note:a", 0, &[event])
                .await
                .expect("append");
            StatusCode::NO_CONTENT
        }),
    );
    // The test's runtime has one thread, so the server's tasks report here.
    let spans = Spans::default();
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(spans.clone()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("address");
    let router = serve_router(routes, Some(store));
    tokio::spawn(async move { axum::serve(listener, router).await });

    let answer = reqwest::Client::new()
        .post(format!("http://{addr}/write"))
        .send()
        .await
        .expect("an answer");
    assert_eq!(
        answer.status(),
        StatusCode::NO_CONTENT,
        "the write is confirmed"
    );

    let seen = spans.0.lock().expect("spans");
    let push = seen
        .iter()
        .find(|span| {
            span.name == "turso.sync.push" && span.parent.as_deref() == Some("http.request")
        })
        .unwrap_or_else(|| panic!("the write's push is inside its request span: {seen:?}"));
    for field in ["waited_ms", "carried"] {
        assert!(
            push.fields.contains(field),
            "the push records {field}: {push:?}"
        );
    }
}

/// What only this server holds when it starts, such as a write from its
/// last run or an index the start built, reaches the remote without
/// waiting for a write: the upkeep pushes it as the server serves.
#[tokio::test]
async fn a_server_pushes_what_its_start_holds_without_a_write() {
    use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};

    let Some(server) = SyncServer::start().await else {
        return;
    };
    let synced = |dir: &tempfile::TempDir| {
        let local = format!("file:{}", dir.path().join("app.db").display());
        let url = server.url.clone();
        async move {
            TursoEventStore::new_synced(&local, &url, "the-local-server-ignores-it")
                .await
                .expect("open the synced store")
        }
    };
    let dir = tempfile::tempdir().expect("data dir");
    let store = synced(&dir).await;
    let event = PersistenceEnvelope {
        sequence_nr: 0,
        event_type: "Written".to_string(),
        payload: serde_json::json!({ "n": 1 }),
        metadata: EventMetadata {
            event_id: uuid::Uuid::new_v4(),
            causation_id: uuid::Uuid::new_v4(),
            correlation_id: uuid::Uuid::new_v4(),
            timestamp: chrono::Utc::now(),
            actor_id: "serve-test".to_string(),
        },
    };
    store
        .append("default:Note:left", 0, &[event])
        .await
        .expect("append");
    drop(store);

    let store = synced(&dir).await;
    let copy = store.sync_copy().expect("a synced store").clone();
    let _routes = serve_router(Router::new(), Some(store));
    let started = Instant::now();
    while copy.remote_record_unsure() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the server pushed nothing in 10 s"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let other = tempfile::tempdir().expect("second machine");
    let restored = synced(&other).await;
    let events = restored
        .read_events("default:Note:left", 0)
        .await
        .expect("read the restored copy");
    assert_eq!(events.len(), 1, "the write left behind is on the remote");
}

#[test]
fn a_question_to_cedar_skips_the_push() {
    // A page's offers after a move waited 34-39 ms here for a push they
    // never needed (production trace 01a1835002c87215af29a4fd2486757b:
    // 0.56 ms of Cedar inside a 39 ms answer).
    assert!(reads_only(&Method::POST, "/api/authorize"));
    // The audit endpoint writes a trajectory row, so it still waits.
    assert!(!reads_only(&Method::POST, "/api/audit"));
    assert!(!reads_only(&Method::POST, "/api/authorize/x"));
    assert!(!reads_only(&Method::PUT, "/api/authorize"));
}

#[test]
fn only_a_copying_database_runs_requests_that_can_change_data() {
    assert!(refusal(&CopyState::Copying).is_none());
    for state in [
        CopyState::Unchecked,
        CopyState::Behind,
        CopyState::Stopped("refused".to_string()),
    ] {
        let refused = refusal(&state).expect("refused");
        assert_eq!(
            refused.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "{state:?}"
        );
    }
}
