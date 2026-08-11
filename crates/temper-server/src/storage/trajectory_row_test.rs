//! Bounds and JSON validity of the captured trajectory request body.

use super::*;

fn entry_with_body(request_body: Option<serde_json::Value>) -> TrajectoryEntry {
    TrajectoryEntry {
        timestamp: "2026-08-11T00:00:00Z".to_string(),
        tenant: "default".to_string(),
        entity_type: "Order".to_string(),
        entity_id: "ord-1".to_string(),
        action: "AddItem".to_string(),
        success: true,
        from_status: Some("Draft".to_string()),
        to_status: Some("Draft".to_string()),
        error: None,
        agent_id: None,
        session_id: None,
        authz_denied: None,
        denied_resource: None,
        denied_module: None,
        source: Some(TrajectorySource::Entity),
        spec_governed: Some(true),
        agent_type: None,
        request_body,
        intent: None,
        matched_policy_ids: None,
    }
}

#[test]
fn small_request_body_is_stored_verbatim() {
    let body = serde_json::json!({"ProductId": "p-1", "Quantity": 2});
    let stored =
        trajectory_request_body_json(&entry_with_body(Some(body.clone()))).expect("stored");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&stored).expect("valid json"),
        body
    );
}

#[test]
fn absent_request_body_stores_nothing() {
    assert!(trajectory_request_body_json(&entry_with_body(None)).is_none());
}

#[test]
fn oversized_request_body_stays_parseable_json() {
    let body = serde_json::json!({"Notes": "x".repeat(20_000)});
    let stored = trajectory_request_body_json(&entry_with_body(Some(body))).expect("stored");

    assert!(
        stored.len() <= TRAJECTORY_REQUEST_BODY_MAX_BYTES,
        "stored body must respect the cap, got {} bytes",
        stored.len()
    );
    let parsed: serde_json::Value =
        serde_json::from_str(&stored).expect("truncated body must still parse as JSON");
    assert_eq!(parsed["_truncated"], serde_json::json!(true));
    assert!(
        parsed["_original_bytes"].as_u64().expect("original bytes") > 20_000,
        "envelope records the pre-truncation size"
    );
    assert!(
        parsed["_preview"]
            .as_str()
            .expect("preview")
            .starts_with("{\"Notes\":\"xxx"),
        "envelope keeps a readable prefix of the original body"
    );
}

#[test]
fn oversized_multibyte_request_body_is_not_split_mid_character() {
    // A body made entirely of 4-byte characters: naive byte slicing at the
    // cap would land mid-character and produce invalid UTF-8/JSON.
    let body = serde_json::json!({"Notes": "\u{1F600}".repeat(5_000)});
    let stored = trajectory_request_body_json(&entry_with_body(Some(body))).expect("stored");

    assert!(stored.len() <= TRAJECTORY_REQUEST_BODY_MAX_BYTES);
    let parsed: serde_json::Value =
        serde_json::from_str(&stored).expect("multibyte truncation must still parse");
    assert_eq!(parsed["_truncated"], serde_json::json!(true));
}

#[test]
fn escape_heavy_request_body_still_fits_the_cap() {
    // Quotes and backslashes double under JSON escaping, so a preview cut
    // at the raw cap would render past it.
    let body = serde_json::json!({"Notes": "\"\\".repeat(6_000)});
    let stored = trajectory_request_body_json(&entry_with_body(Some(body))).expect("stored");

    assert!(
        stored.len() <= TRAJECTORY_REQUEST_BODY_MAX_BYTES,
        "escape expansion must not push the envelope past the cap, got {} bytes",
        stored.len()
    );
    serde_json::from_str::<serde_json::Value>(&stored).expect("valid json");
}

#[test]
fn capture_time_bounding_passes_small_bodies_through_unchanged() {
    let body = serde_json::json!({"ProductId": "p-1", "Quantity": 2});
    assert_eq!(bounded_request_body(&body), body);
}

#[test]
fn capture_time_bounding_caps_oversized_bodies_before_the_outbox() {
    let body = serde_json::json!({"Notes": "x".repeat(20_000)});
    let bounded = bounded_request_body(&body);

    assert_eq!(bounded["_truncated"], serde_json::json!(true));
    assert!(
        bounded.to_string().len() <= TRAJECTORY_REQUEST_BODY_MAX_BYTES,
        "a captured entry must never carry more than the cap into the outbox"
    );
}

#[test]
fn capture_time_bounding_and_sink_encoding_agree() {
    // Whether a body is bounded at capture or at the sink, the stored column
    // must come out identical — otherwise the same action yields two shapes.
    let body = serde_json::json!({"Notes": "y".repeat(20_000)});
    let via_sink =
        trajectory_request_body_json(&entry_with_body(Some(body.clone()))).expect("sink");
    let via_capture =
        trajectory_request_body_json(&entry_with_body(Some(bounded_request_body(&body))))
            .expect("capture");
    assert_eq!(via_sink, via_capture);
}
