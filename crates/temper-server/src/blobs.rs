//! Internal blob storage endpoint and field-overflow helpers.
//!
//! New writes go through the Temper object-store boundary. Turso DB blobs are
//! read only as a legacy fallback for data written by older releases.

use axum::body::Bytes;
use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::Value;
use temper_authz::AuthenticatedRequestContext;

use crate::blob_store::BlobStore;
use crate::state::ServerState;

mod descriptor;
mod hydration;
pub use descriptor::{
    FieldOverflowDescriptor, field_overflow_descriptor, field_overflow_sha256,
    is_valid_field_overflow_key,
};
pub(crate) use hydration::{
    BlobHydrationBudget, hydrate_blob_refs_for_tenant, hydrate_blob_refs_for_tenant_with_budget,
};
#[cfg(test)]
pub(crate) use hydration::{hydrate_blob_refs_in_value, hydrate_blob_refs_in_value_with_ceiling};

pub(crate) const FIELD_OVERFLOW_BLOB_PREFIX: &str = "field-overflow/sha256/";
pub(crate) const FIELD_OVERFLOW_REF_KEY: &str = "__temper_blob_ref";
pub(crate) const FIELD_OVERFLOW_SIZE_KEY: &str = "__temper_blob_size";
pub(crate) const FIELD_OVERFLOW_ENCODING_KEY: &str = "__temper_blob_encoding";

fn require_blob_object_authorization(
    state: &ServerState,
    authenticated: &AuthenticatedRequestContext,
    action: &str,
    key: &str,
) -> Result<(), StatusCode> {
    state
        .authorize_with_context(
            authenticated.security_context(),
            action,
            "BlobObject",
            &std::collections::BTreeMap::from([(
                "id".to_string(),
                serde_json::Value::String(key.to_string()),
            )]),
            authenticated.tenant().as_str(),
        )
        .map_err(|denial| {
            tracing::warn!(
                reason = %denial,
                tenant = %authenticated.tenant(),
                principal_id = %authenticated.security_context().principal.id,
                key,
                action,
                "internal blob object access denied"
            );
            StatusCode::FORBIDDEN
        })
}

#[derive(Debug, Clone)]
pub struct OverflowBlobWrite {
    pub key: String,
    pub body: Vec<u8>,
    /// Optional per-field TTL carried from IOA spec
    /// (`overflow_ttl_seconds`, ADR-0047). `None` means permanent.
    pub ttl_seconds: Option<u64>,
}

pub(crate) async fn put_overflow_blobs(
    store: &BlobStore,
    blobs: &[OverflowBlobWrite],
) -> Result<(), String> {
    for blob in blobs {
        let ttl = blob.ttl_seconds.map(std::time::Duration::from_secs);
        put_blob_bytes_with_ttl(store, &blob.key, &blob.body, ttl).await?;
    }
    Ok(())
}

/// TTL-aware variant of `put_blob_bytes` (ADR-0047).
pub(crate) async fn put_blob_bytes_with_ttl(
    store: &BlobStore,
    key: &str,
    body: &[u8],
    ttl: Option<std::time::Duration>,
) -> Result<(), String> {
    store.put_if_absent(key, body, ttl).await
}

pub(crate) fn blob_ref_value(key: &str, size_bytes: usize) -> Value {
    serde_json::json!({
        FIELD_OVERFLOW_REF_KEY: key,
        FIELD_OVERFLOW_SIZE_KEY: size_bytes,
        FIELD_OVERFLOW_ENCODING_KEY: "json",
    })
}

/// `PUT /_internal/blobs/{*path}` — store a blob.
pub async fn put_blob(
    State(state): State<ServerState>,
    Extension(authenticated): Extension<AuthenticatedRequestContext>,
    Path(path): Path<String>,
    body: Bytes,
) -> impl IntoResponse {
    let tenant = authenticated.tenant();
    if let Err(status) =
        require_blob_object_authorization(&state, &authenticated, "write_blob_object", &path)
    {
        return status.into_response();
    }

    match state.put_blob_object(tenant, &path, &body, None).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            tracing::error!(error = %e, path = %path, "blob put failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e).into_response()
        }
    }
}

/// `GET /_internal/blobs/{*path}` — retrieve a blob.
pub async fn get_blob(
    State(state): State<ServerState>,
    Extension(authenticated): Extension<AuthenticatedRequestContext>,
    Path(path): Path<String>,
) -> impl IntoResponse {
    let tenant = authenticated.tenant();
    if let Err(status) =
        require_blob_object_authorization(&state, &authenticated, "read_blob_object", &path)
    {
        return status.into_response();
    }

    match state.get_blob_with_legacy_fallback(tenant, &path).await {
        Ok(Some(data)) => (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
            data,
        )
            .into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!(error = %e, path = %path, "blob get failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity_actor::effects::{DEFAULT_FIELD_INLINE_MAX, FieldSyncMode, sync_fields};
    use crate::entity_actor::types::EntityState;
    use serde_json::json;
    use tempfile::TempDir;

    async fn open_store() -> (BlobStore, TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = BlobStore::local_fs(dir.path().join("objects"));
        (store, dir)
    }

    fn make_state(entity_type: &str, entity_id: &str) -> EntityState {
        EntityState {
            entity_type: entity_type.into(),
            entity_id: entity_id.into(),
            status: "Initial".into(),
            item_count: 0,
            counters: std::collections::BTreeMap::new(),
            booleans: std::collections::BTreeMap::new(),
            lists: std::collections::BTreeMap::new(),
            fields: json!({}),
            events: std::collections::VecDeque::new(),
            total_event_count: 0,
            events_since_snapshot: 0,
            last_snapshot_sequence_nr: 0,
            sequence_nr: 0,
            processed_idempotency_keys: std::collections::BTreeMap::new(),
        }
    }

    /// End-to-end: sync_fields emits OverflowBlobWrite for a >128KB field;
    /// put_overflow_blobs persists it; hydrate_blob_refs_in_value reconstitutes
    /// the value from storage. This is the OData read-path round trip.
    #[tokio::test]
    async fn blob_overflow_round_trip_through_turso() {
        let (store, _dir) = open_store().await;
        let mut state = make_state("Session", "s-e2e-1");
        let big = "A".repeat(200 * 1024); // 200 KB — above 128 KB ceiling
        let params = json!({ "user_message": big.clone() });

        // Write path: project into fields, collect overflow blobs.
        let overflow = sync_fields(&mut state, &params, FieldSyncMode::blob_refs_default());
        assert_eq!(overflow.len(), 1, "200KB field overflows to blob");
        put_overflow_blobs(&store, &overflow)
            .await
            .expect("put_overflow_blobs");

        // Confirm the field now holds a blob ref envelope, not the raw value.
        let ref_obj = state
            .fields
            .get("user_message")
            .and_then(|v| v.as_object())
            .expect("blob ref object present");
        assert!(ref_obj.contains_key(FIELD_OVERFLOW_REF_KEY));

        // Read path (OData side): fully hydrate.
        let mut hydrated_fields = state.fields.clone();
        hydrate_blob_refs_in_value(&store, &mut hydrated_fields).await;
        let recovered = hydrated_fields
            .get("user_message")
            .and_then(|v| v.as_str())
            .expect("hydrated user_message is a string");
        assert_eq!(recovered, big, "hydrated value matches original bytes");
    }

    /// End-to-end: ceiling-aware hydration inlines small blob refs and defers
    /// large ones into the returned map. This is the WASM-dispatch path from
    /// ADR-0046.
    #[tokio::test]
    async fn hydrate_with_ceiling_inlines_small_and_defers_large() {
        let (store, _dir) = open_store().await;
        let mut state = make_state("Session", "s-e2e-2");

        // Both fields exceed the inline ceiling, but the first is just above
        // (overflowed) and the second is well above. The ceiling parameter to
        // hydrate is what decides inline-vs-defer on read; here we pass a
        // higher ceiling on read than was used on write so the overflow value
        // can be inlined back.
        let just_over_write_ceiling = "B".repeat(150 * 1024); // 150 KB, overflows on write (128KB write ceiling)
        let huge = "C".repeat(300 * 1024); // 300 KB, should stay deferred on read

        let params = json!({
            "medium_field": just_over_write_ceiling.clone(),
            "huge_field": huge.clone(),
        });
        let overflow = sync_fields(&mut state, &params, FieldSyncMode::blob_refs_default());
        assert_eq!(overflow.len(), 2);
        put_overflow_blobs(&store, &overflow)
            .await
            .expect("put_overflow_blobs");

        let mut hydrated_fields = state.fields.clone();
        // Read-side ceiling of 200KB: 150KB field inlines, 300KB stays deferred.
        let deferred =
            hydrate_blob_refs_in_value_with_ceiling(&store, &mut hydrated_fields, 200 * 1024).await;

        // medium_field was inlined.
        assert_eq!(
            hydrated_fields
                .get("medium_field")
                .and_then(|v| v.as_str())
                .map(str::len),
            Some(150 * 1024),
            "medium field inlined as plain string"
        );
        // huge_field remains a blob ref.
        assert!(
            hydrated_fields
                .get("huge_field")
                .and_then(|v| v.as_object())
                .map(|o| o.contains_key(FIELD_OVERFLOW_REF_KEY))
                .unwrap_or(false),
            "huge field still a blob ref in the hydrated fields object"
        );
        // Deferred map contains exactly the huge blob.
        assert_eq!(deferred.len(), 1, "one deferred blob");
        let deferred_bytes = deferred.values().next().unwrap();
        // The blob body is the JSON serialization of the original String value,
        // so it includes the surrounding quotes.
        let decoded: serde_json::Value =
            serde_json::from_slice(deferred_bytes).expect("deferred bytes decode as JSON");
        assert_eq!(decoded.as_str().unwrap(), huge);
    }

    /// Per-field `overflow_inline_max_bytes` in the spec overrides the mode
    /// default. Declaring a 1024-byte ceiling forces a 4KB field into the
    /// overflow path even though the default 128KB ceiling would keep it
    /// inline. ADR-0045 Phase 4b.
    #[tokio::test]
    async fn per_field_inline_max_override_forces_overflow() {
        use crate::entity_actor::effects::sync_fields_with_metadata;
        use std::collections::BTreeMap;
        use temper_jit::table::StateVarMetadata;

        let (store, _dir) = open_store().await;
        let mut state = make_state("Session", "s-metadata-1");
        let value = "Z".repeat(4 * 1024); // 4 KB — normally inline, but override forces overflow
        let params = json!({ "tiny": value });

        let mut metadata = BTreeMap::new();
        metadata.insert(
            "tiny".to_string(),
            StateVarMetadata {
                var_type: None,
                entity_type: None,
                overflow_inline_max_bytes: Some(1024),
                overflow_ttl_seconds: None,
            },
        );

        let overflow = sync_fields_with_metadata(
            &mut state,
            &params,
            crate::entity_actor::effects::FieldSyncMode::blob_refs_default(),
            Some(&metadata),
        );
        assert_eq!(
            overflow.len(),
            1,
            "per-field 1KB ceiling overrides default 128KB ceiling"
        );
        assert!(
            overflow[0].ttl_seconds.is_none(),
            "no TTL declared → permanent"
        );

        put_overflow_blobs(&store, &overflow)
            .await
            .expect("put_overflow_blobs");
    }

    /// Per-field `overflow_ttl_seconds` propagates through `OverflowBlobWrite`
    /// into object-store persistence without requiring a DB blob row.
    /// Object-store retention is delegated to the provider lifecycle policy.
    #[tokio::test]
    async fn per_field_ttl_persists_to_object_store() {
        use crate::entity_actor::effects::sync_fields_with_metadata;
        use std::collections::BTreeMap;
        use temper_jit::table::StateVarMetadata;

        let (store, _dir) = open_store().await;
        let mut state = make_state("WebQuery", "wq-ttl-1");
        // 200 KB — well over default ceiling, so overflow fires.
        let value = "P".repeat(200 * 1024);
        let params = json!({ "results": value });

        let mut metadata = BTreeMap::new();
        metadata.insert(
            "results".to_string(),
            StateVarMetadata {
                var_type: None,
                entity_type: None,
                overflow_inline_max_bytes: None, // use default 128KB ceiling
                overflow_ttl_seconds: Some(1),   // 1-second TTL
            },
        );

        let overflow = sync_fields_with_metadata(
            &mut state,
            &params,
            crate::entity_actor::effects::FieldSyncMode::blob_refs_default(),
            Some(&metadata),
        );
        assert_eq!(overflow.len(), 1);
        assert_eq!(
            overflow[0].ttl_seconds,
            Some(1),
            "ttl propagates from StateVarMetadata into OverflowBlobWrite"
        );

        put_overflow_blobs(&store, &overflow)
            .await
            .expect("put_overflow_blobs");

        // The blob should exist immediately.
        let key = &overflow[0].key;
        assert!(
            store.get(key).await.unwrap().is_some(),
            "freshly-written blob is readable"
        );
    }

    /// Regression test for the replay-path blob-persist bug: a blob written by
    /// a live action must remain resolvable after an actor restart. Simulates
    /// the sequence by writing a blob on behalf of a Session, then re-running
    /// `sync_fields` (as replay would) and persisting the returned overflow
    /// blobs (the fix). Content-addressed dedup means this is idempotent.
    #[tokio::test]
    async fn replay_persists_overflow_blobs_and_hydration_resolves() {
        let (store, _dir) = open_store().await;

        // Live path: first action projects a 200KB value, produces overflow, persists it.
        let mut state_live = make_state("Session", "s-replay-1");
        let big = "R".repeat(200 * 1024);
        let params = json!({ "user_message": big.clone() });
        let overflow = sync_fields(&mut state_live, &params, FieldSyncMode::blob_refs_default());
        put_overflow_blobs(&store, &overflow)
            .await
            .expect("put_overflow_blobs live");
        assert_eq!(overflow.len(), 1, "live action writes one overflow blob");

        // Now simulate replay: fresh EntityState, sync_fields re-runs with the
        // same event.params, returns overflow. BEFORE the fix this return was
        // discarded (`let _ =`), leaving fields with a blob-ref envelope that
        // pointed to nothing after the blob was dropped. With the fix, replay
        // re-persists via put_overflow_blobs (idempotent on content-addressed).
        let mut state_replay = make_state("Session", "s-replay-1");
        let replay_overflow = sync_fields(
            &mut state_replay,
            &params,
            FieldSyncMode::blob_refs_default(),
        );
        assert_eq!(replay_overflow.len(), 1);
        assert_eq!(
            replay_overflow[0].key, overflow[0].key,
            "content-addressed dedup: same key both times"
        );
        // This is the fix — persist on replay.
        put_overflow_blobs(&store, &replay_overflow)
            .await
            .expect("put_overflow_blobs replay (idempotent INSERT OR IGNORE)");

        // Hydration must succeed against the replayed state.
        let mut hydrated = state_replay.fields.clone();
        hydrate_blob_refs_in_value(&store, &mut hydrated).await;
        let recovered = hydrated
            .get("user_message")
            .and_then(|v| v.as_str())
            .expect("user_message hydrated as string after replay");
        assert_eq!(recovered, big, "blob content intact after replay-persist");
    }

    /// Regression test for the inverse case: a replayed overflow blob on an
    /// entity that has no live blob written (the prior server died between
    /// event persist and blob persist). The replay path must write the blob
    /// so that hydration recovers the value.
    #[tokio::test]
    async fn replay_recovers_blob_when_live_write_was_dropped() {
        let (store, _dir) = open_store().await;

        // Simulate: live action projected params into fields but died before
        // put_overflow_blobs completed. On replay we get the same event.params.
        let mut state = make_state("Session", "s-replay-2");
        let big = "S".repeat(200 * 1024);
        let params = json!({ "user_message": big.clone() });
        let overflow = sync_fields(&mut state, &params, FieldSyncMode::blob_refs_default());
        // NOTE: skip put_overflow_blobs here — simulates the server dying
        // before the blob persisted.

        // Now the fields has a blob-ref envelope, but the blob store is empty.
        // Hydration before replay-persist would log "missing" and leave the
        // envelope in place:
        let mut hydrated_before = state.fields.clone();
        hydrate_blob_refs_in_value(&store, &mut hydrated_before).await;
        assert!(
            hydrated_before
                .get("user_message")
                .and_then(|v| v.as_object())
                .is_some(),
            "before replay-persist: blob ref remains an object (no backing blob)"
        );

        // Replay path (the fix): persist the overflow blobs on replay.
        put_overflow_blobs(&store, &overflow)
            .await
            .expect("replay-persist recovers the lost write");

        // Now hydration succeeds.
        let mut hydrated_after = state.fields.clone();
        hydrate_blob_refs_in_value(&store, &mut hydrated_after).await;
        let recovered = hydrated_after
            .get("user_message")
            .and_then(|v| v.as_str())
            .expect("user_message hydrated after replay-persist");
        assert_eq!(recovered, big);
    }

    /// End-to-end: when the default ceiling is used on read (the dispatcher's
    /// path), every field whose stored size exceeds `DEFAULT_FIELD_INLINE_MAX`
    /// lands in the deferred map and the fields object keeps ref envelopes.
    #[tokio::test]
    async fn hydrate_at_default_ceiling_defers_oversize_for_wasm() {
        let (store, _dir) = open_store().await;
        let mut state = make_state("Session", "s-e2e-3");
        let big = "D".repeat(200 * 1024); // > 128 KB default ceiling
        let params = json!({ "huge": big.clone() });
        let overflow = sync_fields(&mut state, &params, FieldSyncMode::blob_refs_default());
        put_overflow_blobs(&store, &overflow)
            .await
            .expect("put_overflow_blobs");

        let mut hydrated_fields = state.fields.clone();
        let deferred = hydrate_blob_refs_in_value_with_ceiling(
            &store,
            &mut hydrated_fields,
            DEFAULT_FIELD_INLINE_MAX,
        )
        .await;

        assert_eq!(deferred.len(), 1, "oversize field goes to deferred map");
        assert!(
            hydrated_fields
                .get("huge")
                .and_then(|v| v.as_object())
                .map(|o| o.contains_key(FIELD_OVERFLOW_REF_KEY))
                .unwrap_or(false),
            "fields retains blob-ref envelope (WASM guest resolves via host_read_field)"
        );
    }
}

#[cfg(test)]
#[path = "blobs/read_budget_tests.rs"]
mod read_budget_tests;

#[cfg(test)]
#[path = "blobs/endpoint_tests.rs"]
mod endpoint_tests;
