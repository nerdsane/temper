//! Follow real nextLinks across unknown legacy metadata and known values.
use super::*;
use serde_json::{Value, json};

#[tokio::test]
async fn turso_metadata_nextlinks_cross_null_boundaries_in_both_directions() {
    let dir = tempfile::tempdir().unwrap();
    let store = TursoEventStore::new(
        &format!("file:{}", dir.path().join("metadata.db").display()),
        None,
    )
    .await
    .unwrap();
    for (id, at, via, by) in [
        ("a", None, None, None),
        (
            "b",
            Some("2026-01-01T00:00:00.200Z"),
            Some("request"),
            Some("zoe"),
        ),
        (
            "c",
            Some("2026-01-01T00:00:00.100Z"),
            Some("request"),
            Some("ann"),
        ),
        ("d", None, None, None),
        (
            "e",
            Some("2026-01-01T00:00:00.100Z"),
            Some("request"),
            Some("ann"),
        ),
    ] {
        // Legacy catalog rows with no first event are intentionally null.
        // Tied known and unknown values both require the id tiebreaker.
        store
            .upsert_query_projection_with_state(
                "default",
                "Order",
                id,
                "Draft",
                &json!({"Id": id}),
                &json!({"first_event_at": at, "last_event_at": at,
                "last_event_via": via, "last_event_by": by}),
                2,
            )
            .await
            .unwrap();
    }
    let state = build_turso_state("metadata-nextlinks", store);
    install_order_crud_test_policy(&state);
    for property in [
        "first_event_at",
        "last_event_at",
        "last_event_via",
        "last_event_by",
    ] {
        for direction in ["asc", "desc"] {
            let expected = match (property, direction) {
                ("last_event_via", "asc") => vec!["b", "c", "e", "a", "d"],
                ("last_event_via", _) => vec!["a", "d", "b", "c", "e"],
                (_, "asc") => vec!["c", "e", "b", "a", "d"],
                _ => vec!["a", "d", "b", "c", "e"],
            };
            for top in [1, 2] {
                let mut path = format!(
                    "/tdata/Orders?$orderby={property}%20{direction}&$top={top}&$select=entity_id"
                );
                let mut actual = Vec::new();
                for page in 0..expected.len() {
                    let (status, body) = get_json(&state, &path).await;
                    assert_eq!(status, StatusCode::OK, "{path}: {body}");
                    let rows = body["value"].as_array().unwrap();
                    assert!(!rows.is_empty(), "{path}: empty continuation");
                    for row in rows {
                        assert!(row.get(property).is_none(), "$select omits the cursor key");
                        actual.push(row["entity_id"].as_str().unwrap().to_owned());
                    }
                    match body.get("@odata.nextLink").and_then(Value::as_str) {
                        Some(next) => {
                            assert!(page + 1 < expected.len(), "{path}: continuation loop");
                            path = format!("/tdata/{next}");
                        }
                        None => break,
                    }
                }
                assert_eq!(actual, expected, "{property} {direction}, top {top}");
            }
        }
    }
}
