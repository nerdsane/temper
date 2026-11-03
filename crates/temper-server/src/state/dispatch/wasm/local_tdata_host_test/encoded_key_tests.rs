//! A guest's loopback `/tdata` path is percent-decoded once, as the HTTP route
//! decodes it, so an encoded key names the same record either way.

use super::*;

#[test]
fn a_local_tdata_path_is_percent_decoded_like_the_http_route() {
    let request = LocalTDataRequest::parse(
        "http://127.0.0.1:8787/tdata/Files('doc%207')/$value",
        &BTreeSet::new(),
    )
    .expect("loopback TData URL should parse");

    assert_eq!(request.path, "Files('doc 7')/$value");
}

#[tokio::test]
async fn an_encoded_key_reaches_the_record_it_names() {
    let state = test_state();
    permit_agents(&state);
    let agent = test_agent();
    let host = LocalTDataWasmHost::new(
        state,
        temper_runtime::tenant::TenantId::default(),
        Some(&agent),
        "test-module",
        Arc::new(FailingHost),
    );
    let headers = vec![
        ("content-type".to_string(), "application/json".to_string()),
        ("accept".to_string(), "application/json".to_string()),
    ];
    for (id, customer) in [("order 1", "Ada"), ("order%201", "Lin")] {
        let body = serde_json::json!({ "id": id, "Customer": customer }).to_string();
        let (status, _) = host
            .http_call(
                "POST",
                "http://127.0.0.1:8787/tdata/Orders",
                &headers,
                &body,
            )
            .await
            .expect("local create should dispatch");
        assert_eq!(status, StatusCode::CREATED.as_u16(), "create {id}");
    }

    let (status, body) = host
        .http_call(
            "GET",
            "http://127.0.0.1:8787/tdata/Orders('order%201')",
            &headers,
            "",
        )
        .await
        .expect("local read should dispatch");
    assert_eq!(status, StatusCode::OK.as_u16());
    let fetched: serde_json::Value = serde_json::from_str(&body).expect("fetched JSON");
    assert_eq!(fetched["entity_id"], "order 1");
    assert_eq!(fetched["fields"]["Customer"], "Ada");

    let (status, body) = host
        .http_call(
            "POST",
            "http://127.0.0.1:8787/tdata/Orders('order%201')/Temper.Local.SubmitOrder",
            &headers,
            "{}",
        )
        .await
        .expect("local action should dispatch");
    assert_eq!(status, StatusCode::OK.as_u16());
    let submitted: serde_json::Value = serde_json::from_str(&body).expect("action JSON");
    assert_eq!(submitted["status"], "Submitted");

    // The record whose id is literally `order%201` is reached by encoding its
    // `%`, and the action above left it as it was.
    let (status, body) = host
        .http_call(
            "GET",
            "http://127.0.0.1:8787/tdata/Orders('order%25201')",
            &headers,
            "",
        )
        .await
        .expect("local read should dispatch");
    assert_eq!(status, StatusCode::OK.as_u16());
    let other: serde_json::Value = serde_json::from_str(&body).expect("fetched JSON");
    assert_eq!(other["entity_id"], "order%201");
    assert_eq!(other["fields"]["Customer"], "Lin");
    assert_eq!(other["status"], "Draft");
}

#[tokio::test]
async fn a_local_span_keeps_decoded_question_marks_without_the_query() {
    use std::sync::Mutex;
    use tracing::field::{Field, Visit};
    use tracing::instrument::WithSubscriber;
    use tracing_subscriber::prelude::*;

    #[derive(Clone)]
    struct Paths(Arc<Mutex<Vec<String>>>);

    impl Visit for Paths {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            if field.name() == "url.path" {
                self.0
                    .lock()
                    .expect("paths lock")
                    .push(format!("{value:?}"));
            }
        }
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Paths {
        fn on_new_span(
            &self,
            attrs: &tracing::span::Attributes<'_>,
            _id: &tracing::span::Id,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if attrs.metadata().name() == "wasm.local_tdata_http_call" {
                attrs.record(&mut self.clone());
            }
        }
    }

    // tracing-core's single-dispatcher fast path uses the current thread's
    // subscriber when a callsite is first registered. Keep a second dispatcher
    // alive so a parallel test without a subscriber cannot cache `never` here.
    let _other_dispatch = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
    let paths = Paths(Arc::new(Mutex::new(Vec::new())));
    let subscriber = tracing_subscriber::registry().with(paths.clone());
    let dispatch = tracing::Dispatch::new(subscriber);
    let state = test_state();
    permit_agents(&state);
    let agent = test_agent();
    let host = LocalTDataWasmHost::new(
        state,
        temper_runtime::tenant::TenantId::default(),
        Some(&agent),
        "test-module",
        Arc::new(FailingHost),
    );
    let url = "http://localhost:8787/tdata/Files('doc%3F7')/$value?secret=query-secret";
    let request = LocalTDataRequest::parse(url, &BTreeSet::new()).expect("local URL");
    assert_eq!(request.path, "Files('doc?7')/$value");
    assert_eq!(
        request.query.get("secret").map(String::as_str),
        Some("query-secret")
    );
    host.http_call("GET", url, &[], "")
        .with_subscriber(dispatch)
        .await
        .expect("local dispatch");
    assert_eq!(
        *paths.0.lock().expect("paths lock"),
        ["Files('doc?7')/$value"]
    );
}
