use super::*;

#[test]
fn projection_sequence_conversions_reject_overflow_and_negative_storage() {
    assert_eq!(
        turso_projection_sequence(i64::MAX as u64, "test").unwrap(),
        i64::MAX
    );
    assert!(turso_projection_sequence(i64::MAX as u64 + 1, "test").is_err());
    assert!(decoded_projection_sequence(-1, "bad").is_err());
}

#[test]
fn scalar_to_text_converts_primitives() {
    assert_eq!(
        scalar_to_text(&serde_json::json!("hello")),
        Some("hello".to_string())
    );
    assert_eq!(
        scalar_to_text(&serde_json::json!(42)),
        Some("42".to_string())
    );
    assert_eq!(
        scalar_to_text(&serde_json::json!(true)),
        Some("true".to_string())
    );
    assert_eq!(scalar_to_text(&serde_json::Value::Null), None);
}

#[test]
fn scalar_to_text_skips_complex_types() {
    assert_eq!(scalar_to_text(&serde_json::json!({"a": 1})), None);
    assert_eq!(scalar_to_text(&serde_json::json!([1, 2, 3])), None);
}

#[test]
fn indexed_projection_fields_skips_oversized_scalars() {
    let long = "x".repeat(MAX_INDEXABLE_FIELD_VALUE_BYTES + 1);
    let fields = serde_json::json!({
        "Title": "short",
        "Payload": long,
    });

    let indexed = indexed_projection_fields("Active", &fields);

    assert!(
        indexed
            .iter()
            .any(|(name, value)| name == "Title" && value.as_deref() == Some("short"))
    );
    assert!(indexed.iter().all(|(name, _)| name != "Payload"));
    assert!(
        indexed
            .iter()
            .any(|(name, value)| name == "Status" && value.as_deref() == Some("Active"))
    );
}
