//! Unit tests for the keyset paging cursor and continuation predicate.
//!
//! The load-bearing invariant: for any canonical ordering and any boundary row,
//! the keyset predicate selects exactly the rows that sort strictly after that
//! boundary — the same rows the `$orderby` sort would place after it. We check it
//! by sorting a set with the real query evaluator, then asserting the predicate
//! partitions the set at each boundary.

use super::*;
use crate::query_eval::apply_query_options;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use temper_odata::query::types::{OrderByClause, OrderDirection, QueryOptions};

fn row(id: &str, extra: serde_json::Value) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    obj.insert("entity_id".to_string(), serde_json::json!(id));
    if let Some(fields) = extra.as_object() {
        for (k, v) in fields {
            obj.insert(k.clone(), v.clone());
        }
    }
    serde_json::Value::Object(obj)
}

fn ids(rows: &[serde_json::Value]) -> Vec<String> {
    rows.iter()
        .map(|r| r["entity_id"].as_str().unwrap().to_string())
        .collect()
}

fn sorted(rows: &[serde_json::Value], order: &[OrderByClause]) -> Vec<serde_json::Value> {
    let (out, _) = apply_query_options(
        rows.to_vec(),
        &QueryOptions {
            orderby: Some(order.to_vec()),
            ..QueryOptions::default()
        },
    );
    out
}

/// Rows the keyset predicate keeps, in canonical order.
fn after(
    rows: &[serde_json::Value],
    cursor: &Cursor,
    order: &[OrderByClause],
) -> Vec<serde_json::Value> {
    let predicate = keyset_after_predicate(cursor, order).expect("keyset predicate");
    let (kept, _) = apply_query_options(
        rows.to_vec(),
        &QueryOptions {
            filter: Some(predicate),
            orderby: Some(order.to_vec()),
            ..QueryOptions::default()
        },
    );
    kept
}

#[test]
fn cursor_roundtrips_through_the_token() {
    let order = canonical_pagination_order(Some(&[OrderByClause {
        property: "Name".to_string(),
        direction: OrderDirection::Asc,
    }]));
    let boundary = row("en-05", serde_json::json!({ "Name": "Civic Press" }));
    let cursor = cursor_for_row(&boundary, &order);
    let token = encode_cursor(&cursor);
    assert_eq!(decode_cursor(&token).as_ref(), Some(&cursor));
}

#[test]
fn malformed_token_decodes_to_none() {
    assert!(decode_cursor("not*base64*").is_none());
    assert!(decode_cursor("").is_none());
    // Valid base64url of non-array JSON.
    assert!(decode_cursor(&URL_SAFE_NO_PAD.encode(b"{}")).is_none());
}

#[test]
fn canonical_order_appends_entity_id_tiebreaker() {
    let order = canonical_pagination_order(None);
    assert_eq!(order.len(), 1);
    assert_eq!(order[0].property, ID_PROPERTY);

    let with_user = canonical_pagination_order(Some(&[OrderByClause {
        property: "Name".to_string(),
        direction: OrderDirection::Desc,
    }]));
    assert_eq!(with_user.len(), 2);
    assert_eq!(with_user[1].property, ID_PROPERTY);
    assert_eq!(with_user[1].direction, OrderDirection::Asc);

    // Already terminated by entity_id: no duplicate tiebreaker.
    let already = canonical_pagination_order(Some(&[OrderByClause {
        property: ID_PROPERTY.to_string(),
        direction: OrderDirection::Asc,
    }]));
    assert_eq!(already.len(), 1);
}

#[test]
fn keyset_partitions_at_every_boundary_id_only() {
    let order = canonical_pagination_order(None);
    let rows = vec![
        row("en-03", serde_json::json!({})),
        row("en-01", serde_json::json!({})),
        row("en-02", serde_json::json!({})),
    ];
    let ordered = sorted(&rows, &order);
    assert_eq!(ids(&ordered), vec!["en-01", "en-02", "en-03"]);

    for cut in 0..ordered.len() {
        let cursor = cursor_for_row(&ordered[cut], &order);
        let tail = after(&rows, &cursor, &order);
        let expected: Vec<String> = ids(&ordered[cut + 1..]);
        assert_eq!(ids(&tail), expected, "boundary at index {cut}");
    }
}

#[test]
fn keyset_partitions_with_user_orderby_and_ties() {
    let order = canonical_pagination_order(Some(&[OrderByClause {
        property: "Score".to_string(),
        direction: OrderDirection::Desc,
    }]));
    // Ties on Score are broken by entity_id ascending.
    let rows = vec![
        row("en-a", serde_json::json!({ "Score": 10 })),
        row("en-b", serde_json::json!({ "Score": 30 })),
        row("en-c", serde_json::json!({ "Score": 10 })),
        row("en-d", serde_json::json!({ "Score": 20 })),
    ];
    let ordered = sorted(&rows, &order);
    assert_eq!(ids(&ordered), vec!["en-b", "en-d", "en-a", "en-c"]);

    for cut in 0..ordered.len() {
        let cursor = cursor_for_row(&ordered[cut], &order);
        let tail = after(&rows, &cursor, &order);
        assert_eq!(
            ids(&tail),
            ids(&ordered[cut + 1..]),
            "boundary at index {cut}"
        );
    }
}

#[test]
fn keyset_handles_null_orderby_values() {
    // Ascending: nulls sort last. Mix present and absent Score values.
    let order = canonical_pagination_order(Some(&[OrderByClause {
        property: "Score".to_string(),
        direction: OrderDirection::Asc,
    }]));
    let rows = vec![
        row("en-a", serde_json::json!({ "Score": 5 })),
        row("en-b", serde_json::json!({})), // absent -> null, sorts last
        row("en-c", serde_json::json!({ "Score": 8 })),
        row("en-d", serde_json::json!({})), // absent -> null, sorts last, id tiebreak
    ];
    let ordered = sorted(&rows, &order);
    assert_eq!(ids(&ordered), vec!["en-a", "en-c", "en-b", "en-d"]);

    for cut in 0..ordered.len() {
        let cursor = cursor_for_row(&ordered[cut], &order);
        let tail = after(&rows, &cursor, &order);
        assert_eq!(
            ids(&tail),
            ids(&ordered[cut + 1..]),
            "boundary at index {cut}"
        );
    }
}

#[test]
fn explicit_null_cursor_keys_are_absent_for_any_property() {
    for property in ["first_event_at", "last_event_by", "Score"] {
        let order = canonical_pagination_order(Some(&[OrderByClause {
            property: property.into(),
            direction: OrderDirection::Asc,
        }]));
        let explicit = row("en-a", serde_json::json!({ property: null }));
        let missing = row("en-a", serde_json::json!({}));
        let cursor = cursor_for_row(&explicit, &order);
        assert!(!cursor.keys[0].present, "{property}: null is absent");
        assert_eq!(cursor, cursor_for_row(&missing, &order));
        // Continue tokens already issued by the old encoder, too.
        let legacy = URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&serde_json::json!([[true, null], [true, "en-a"]])).unwrap(),
        );
        assert_eq!(decode_cursor(&legacy), Some(cursor.clone()));
        assert_eq!(decode_cursor(&encode_cursor(&cursor)), Some(cursor));
    }
}

#[test]
fn metadata_continuation_partitions_every_null_and_known_boundary_both_directions() {
    for property in [
        "first_event_at",
        "last_event_at",
        "last_event_via",
        "last_event_by",
    ] {
        let (low, high) = match property {
            "first_event_at" | "last_event_at" => {
                ("2026-01-01T00:00:00.100Z", "2026-01-01T00:00:00.200Z")
            }
            "last_event_via" => ("request", "timer"),
            _ => ("ann", "zoe"),
        };
        let rows = vec![
            row("en-a", serde_json::json!({property: null})),
            row("en-b", serde_json::json!({property: high})),
            row("en-c", serde_json::json!({property: low})),
            row("en-d", serde_json::json!({property: null})),
            row("en-e", serde_json::json!({property: low})),
        ];
        for direction in [OrderDirection::Asc, OrderDirection::Desc] {
            let order = canonical_pagination_order(Some(&[OrderByClause {
                property: property.into(),
                direction,
            }]));
            let ordered = sorted(&rows, &order);
            let expected = match direction {
                OrderDirection::Asc => vec!["en-c", "en-e", "en-b", "en-a", "en-d"],
                OrderDirection::Desc => vec!["en-a", "en-d", "en-b", "en-c", "en-e"],
            };
            assert_eq!(ids(&ordered), expected, "{property} {direction:?}");
            for cut in 0..ordered.len() {
                let cursor = cursor_for_row(&ordered[cut], &order);
                let decoded = decode_cursor(&encode_cursor(&cursor)).unwrap();
                assert_eq!(
                    ids(&after(&rows, &decoded, &order)),
                    ids(&ordered[cut + 1..]),
                    "{property} {direction:?}: boundary {cut}"
                );
            }
        }
    }
}
