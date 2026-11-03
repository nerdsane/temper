use super::*;
use axum::{Json, Router, extract::Query, routing::get};

#[tokio::test]
async fn follows_continuation_even_when_page_is_smaller_than_requested() {
    let app = Router::new().route("/tdata/Insights", get(|Query(query): Query<BTreeMap<String, String>>| async move {
        if query.contains_key("$skiptoken") {
            Json(serde_json::json!({"value": [{"entity_id": "bad", "status": "Removed"}]}))
        } else {
            Json(serde_json::json!({"value": [{"entity_id": "good", "status": "Pending"}], "@odata.nextLink": "Insights?$skiptoken=page2"}))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let rows = fetch_rows(&reqwest::Client::new(), &base, "Insights", None, None)
        .await
        .unwrap()
        .unwrap();
    server.abort();
    assert_eq!(rows.len(), 2, "the second page must also be audited");
    assert_eq!(rows[1]["entity_id"], "bad");
}

#[tokio::test]
async fn incomplete_or_unsafe_continuations_fail_the_audit() {
    use axum::{extract::Path, http::StatusCode, response::IntoResponse};
    let app = Router::new().route(
        "/tdata/{set}",
        get(
            |Path(set): Path<String>, Query(query): Query<BTreeMap<String, String>>| async move {
                if set == "Denied" && query.contains_key("$skiptoken") {
                    return StatusCode::FORBIDDEN.into_response();
                }
                let next = match set.as_str() {
                    "Cycle" => "Cycle?$skiptoken=same",
                    "Foreign" => "http://example.invalid/tdata/Foreign",
                    _ => "Denied?$skiptoken=page2",
                };
                Json(serde_json::json!({"value": [], "@odata.nextLink": next})).into_response()
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    for (set, message) in [
        ("Cycle", "repeated"),
        ("Foreign", "cross-origin"),
        ("Denied", "incomplete"),
    ] {
        let error = fetch_rows(
            &reqwest::Client::new(),
            &base,
            set,
            Some("test-token"),
            Some("test-tenant"),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains(message), "{error}");
    }
    server.abort();
}

#[tokio::test]
async fn rejected_tokens_keep_401_and_request_credential_repair() {
    use axum::http::StatusCode;
    let app = Router::new()
        .route("/tdata", get(|| async { StatusCode::UNAUTHORIZED }))
        .route(
            "/tdata/Insights",
            get(|| async { StatusCode::UNAUTHORIZED }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = reqwest::Client::new();
    let document = fetch_entity_sets(&client, &base, Some("expired"), Some("tenant"))
        .await
        .unwrap_err();
    let collection = fetch_rows(&client, &base, "Insights", Some("expired"), Some("tenant"))
        .await
        .unwrap_err();
    server.abort();
    for error in [document, collection] {
        let message = error.to_string();
        assert!(
            message.contains("401") && message.contains("TEMPER_TOKEN"),
            "{message}"
        );
        assert!(
            !message.contains("Cedar") && !message.contains("credentials are not the problem"),
            "{message}"
        );
    }
}

#[test]
fn report_rendering_preserves_findings_and_coverage_warnings() {
    let order =
        parse_automaton(include_str!("../../../test-fixtures/specs/order.ioa.toml")).unwrap();
    let mut empty = order.clone();
    empty.automaton.name = "Empty".into();
    let automata = BTreeMap::from([("Order".into(), order), ("Empty".into(), empty)]);
    let broken = EntitySnapshot::from_tdata_row(&serde_json::json!({
        "entity_id": "order-1", "status": "Submitted", "counters": {"items": 0},
        "booleans": {"has_address": true}, "fields": {}
    }))
    .unwrap();
    let by_type = BTreeMap::from([("Order".into(), vec![broken]), ("Unmatched".into(), vec![])]);
    let refused = "Policies".to_string();
    let (output, violations) = render_reports(&automata, &by_type, &[&refused], 2);
    assert!(violations > 0);
    for expected in [
        "Policies",
        "Unmatched",
        "Empty: 0 entities",
        "nothing was checked",
        "Order: 1 entities, 1 with violations",
        "[VIOLATION] order-1",
        "SubmitRequiresItems",
        "items=0",
        "1 entities audited",
        "2 row(s) could not be read",
    ] {
        assert!(output.contains(expected), "missing {expected}: {output}");
    }
}
