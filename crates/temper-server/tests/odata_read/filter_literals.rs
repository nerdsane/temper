//! A `$filter` string literal reaches the filter parser as the client sent it,
//! after one percent-decode: `&`, `(` and `=` inside the literal are data, not
//! query-string syntax.

use super::*;

/// Orders whose `Notes` hold characters that also delimit a query string.
const LITERAL_ORDERS: [(&str, &str); 4] = [
    ("ord-ampersand", "R&D"),
    ("ord-open-paren", "a (draft"),
    ("ord-equals", "x=y"),
    ("ord-plain", "plain"),
];

async fn literal_state(system_name: &str) -> (ServerState, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("isolated literal database");
    let url = format!("file:{}", dir.path().join("literals.db").display());
    let store = TursoEventStore::new(&url, None).await.unwrap();
    let state = build_turso_state(system_name, store);
    for (id, notes) in LITERAL_ORDERS {
        let (status, body) = post_json(
            &state,
            "/tdata/Orders",
            serde_json::json!({ "id": id, "Notes": notes }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "seed {id}: {body:?}");
    }
    (state, dir)
}

async fn assert_filter_matches(state: &ServerState, query: &str, expected_id: &str) {
    let (status, body) = get_json(state, &format!("/tdata/Orders?{query}")).await;
    assert_eq!(status, StatusCode::OK, "{query}: {body}");
    let ids: Vec<&str> = body["value"]
        .as_array()
        .expect("value array")
        .iter()
        .map(|row| row["entity_id"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(ids, [expected_id], "{query}: {body}");
}

#[tokio::test]
async fn filter_literal_with_ampersand_returns_the_matching_record() {
    let (state, _dir) = literal_state("odata-filter-literal-ampersand").await;
    assert_filter_matches(&state, "$filter=Notes%20eq%20'R%26D'", "ord-ampersand").await;
}

#[tokio::test]
async fn filter_literal_with_unbalanced_paren_returns_the_matching_record() {
    let (state, _dir) = literal_state("odata-filter-literal-paren").await;
    // `$top` sorts after `$filter`, so an open parenthesis carried across the
    // literal would pull `$top` into the filter expression.
    assert_filter_matches(
        &state,
        "$filter=Notes%20eq%20'a%20(draft'&$top=10",
        "ord-open-paren",
    )
    .await;
}

#[tokio::test]
async fn filter_literal_with_equals_returns_the_matching_record() {
    let (state, _dir) = literal_state("odata-filter-literal-equals").await;
    assert_filter_matches(&state, "$filter=Notes%20eq%20'x%3Dy'&$top=10", "ord-equals").await;
}
