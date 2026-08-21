//! Streaming raw-binary ingest for content-addressed entity sets.
//!
//! Some entity sets store opaque binary content where the row's `Id`
//! is a hash of the bytes (currently: git's `Blob` set). The normal
//! OData POST round-trip requires the client to base64-encode the
//! body, wrap it in a JSON document, and send the document — and the
//! server to parse JSON, base64-decode, and write. For multi-MiB
//! payloads that's a significant cost on both sides.
//!
//! `Temper.IngestRaw` skips the encoding round-trip:
//!
//!   * Client streams the raw bytes as the request body.
//!   * Server hashes them on the way past, computes the canonical
//!     `<kind> <length>\0<bytes>` form, persists the row with that
//!     hash as the row Id, and returns the Id.
//!
//! Today this is wired only for `/tdata/Blobs/Temper.IngestRaw`.
//! Other content-addressed sets can opt in by registering the same
//! handler under their prefix; the kind tag and entity_type are
//! parameters of the handler.

use axum::body::Body;
use axum::extract::{Extension, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use futures_util::TryStreamExt as _;
use temper_authz::AuthenticatedRequestContext;
use temper_runtime::scheduler::sim_now;

use super::account_verification::enforce_commons_account_verified_for_write;
use super::authz::{
    CREATE_ACTION, MutationResource, apply_authenticated_context, authorize_mutation,
    require_authenticated_context,
};
use super::common::run_write_prechecks;
use super::response::annotate_entity;
use super::storage_guardrails::storage_cap_error_response;
use crate::blob_store::{BlobByteStream, BlobIngestAdmissionError, MAX_RAW_BLOB_BYTES};
use crate::blobs::{FIELD_OVERFLOW_BLOB_PREFIX, blob_ref_value};
use crate::request_context::extract_agent_context;
use crate::response::{ODataResponse, odata_error};
use crate::state::ServerState;

const EXPECTED_OBJECT_ID_HEADER: &str = "x-expected-object-id";

mod responses;
use responses::{
    blob_store_error_response, remove_binary_fields_from_create_response, stage_error_response,
};

/// `POST /tdata/Blobs/Temper.IngestRaw` — stream raw blob bytes,
/// hash them, persist a `Blob` row keyed by the SHA-1 of the
/// canonical form `blob <len>\0<body>`.
///
/// Required headers:
///   * `Content-Length` — declared body length, used both for the
///     canonical hash prefix and as a defence against open-ended
///     streams.
///   * `X-Repository-Id` — foreign key back to the parent repo.
///   * `X-Expected-Object-Id` — lowercase SHA-1 of the canonical object,
///     required so Cedar and quota admission use the exact resource before
///     the request body is polled.
///
/// Authority and tenant are supplied by the authenticated typed request
/// context, exactly like every protected OData write.
pub async fn handle_blob_ingest_raw(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    headers: HeaderMap,
    body: Body,
) -> impl IntoResponse {
    let authenticated = match require_authenticated_context(authenticated) {
        Ok(context) => context,
        Err(error) => return error.into_response(),
    };
    ingest_raw_inner(state, authenticated, headers, body, "Blob", "blob").await
}

async fn ingest_raw_inner(
    state: ServerState,
    authenticated: AuthenticatedRequestContext,
    headers: HeaderMap,
    body: Body,
    entity_type: &str,
    kind_tag: &str,
) -> axum::response::Response {
    let tenant = authenticated.tenant().clone();
    let security_ctx = authenticated.security_context().clone();
    let mut agent_ctx = extract_agent_context(&headers);
    apply_authenticated_context(&mut agent_ctx, &security_ctx);

    let repository_id = match headers
        .get("x-repository-id")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(s) => s.to_string(),
        None => {
            return odata_error(
                StatusCode::BAD_REQUEST,
                "MissingRepositoryId",
                "X-Repository-Id header required",
            )
            .into_response();
        }
    };

    let expected_object_id = match expected_object_id(&headers) {
        Ok(object_id) => object_id,
        Err(response) => return *response,
    };

    let declared_len = match headers
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<usize>().ok())
    {
        Some(n) => n,
        None => {
            return odata_error(
                StatusCode::LENGTH_REQUIRED,
                "MissingContentLength",
                "Content-Length header required",
            )
            .into_response();
        }
    };
    let now = sim_now().to_rfc3339();
    let admission_fields = serde_json::json!({
        "Id": expected_object_id,
        "RepositoryId": repository_id,
        "Size": declared_len as i64,
        "Status": "Durable",
        "CreatedAt": now,
    });
    let attrs = match state
        .build_create_authz_resource_attrs(
            &tenant,
            entity_type,
            &expected_object_id,
            &admission_fields,
        )
        .await
    {
        Ok(attrs) => attrs,
        Err(error) => {
            return odata_error(StatusCode::INTERNAL_SERVER_ERROR, "ReadError", &error)
                .into_response();
        }
    };
    if let Err(response) = authorize_mutation(
        &state,
        &tenant,
        &security_ctx,
        &agent_ctx,
        CREATE_ACTION,
        MutationResource {
            entity_type,
            entity_id: &expected_object_id,
            attrs: &attrs,
        },
    )
    .await
    {
        return response;
    }

    // Reserve staging capacity only after Cedar admits the exact expected
    // object ID/repository/size. Otherwise denied credentials could occupy the
    // tenant or global upload permits without ever being allowed to poll a body.
    let mut staging_admission = match state
        .raw_blob_ingest_budget
        .try_reserve(&tenant, declared_len)
    {
        Ok(permit) => permit,
        Err(BlobIngestAdmissionError::ObjectTooLarge) => {
            return odata_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "ObjectTooLarge",
                &format!(
                    "declared size {declared_len} exceeds the {} byte object or {} byte staging budget",
                    MAX_RAW_BLOB_BYTES,
                    state.raw_blob_ingest_budget.capacity_bytes()
                ),
            )
            .into_response();
        }
        Err(BlobIngestAdmissionError::BudgetExhausted) => {
            return odata_error(
                StatusCode::TOO_MANY_REQUESTS,
                "BlobIngestBudgetExhausted",
                "Concurrent raw Blob staging has exhausted its admission budget",
            )
            .into_response();
        }
        Err(BlobIngestAdmissionError::TenantBusy) => {
            return odata_error(
                StatusCode::TOO_MANY_REQUESTS,
                "BlobIngestTenantBusy",
                "This tenant already has a raw Blob upload in progress",
            )
            .into_response();
        }
    };

    // Snapshot repository/account/quota admission under the existing commons
    // mutation lock, then keep only the owner-byte reservation across I/O. A
    // slow upload must not hold the coarse cross-tenant lock.
    let admission_guard = state.acquire_commons_write_guardrail_lock(&tenant).await;

    if let Err(response) = run_write_prechecks(
        &state,
        &tenant,
        entity_type,
        &expected_object_id,
        ("Create", "create"),
        &admission_fields,
        None,
    )
    .await
    {
        return response;
    }
    if let Err(response) =
        enforce_commons_account_verified_for_write(&state, &tenant, entity_type, &admission_fields)
            .await
    {
        return *response;
    }
    let mut storage_reservation = match state
        .reserve_commons_blob_storage(
            &tenant,
            &expected_object_id,
            &repository_id,
            declared_len as i64,
        )
        .await
    {
        Ok(reservation) => reservation,
        Err(error) => return storage_cap_error_response(error),
    };
    drop(admission_guard);

    let blob_store = match state.blob_store_for_tenant(&tenant) {
        Ok(store) => store,
        Err(error) => {
            return odata_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "BlobStoreUnavailable",
                &error,
            )
            .into_response();
        }
    };
    let canonical_prefix = format!("{kind_tag} {declared_len}\0");
    let stream: BlobByteStream = Box::pin(
        body.into_data_stream()
            .map_err(|error| std::io::Error::other(error.to_string())),
    );
    let staged = match blob_store
        .stage_canonical_stream(
            stream,
            declared_len,
            canonical_prefix.as_bytes(),
            state.raw_blob_ingest_budget.progress_policy(),
            &mut staging_admission,
        )
        .await
    {
        Ok(staged) => staged,
        Err(error) => return stage_error_response(error),
    };
    if staged.canonical_sha1() != expected_object_id {
        return odata_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "ObjectDigestMismatch",
            &format!(
                "expected object ID {expected_object_id}, computed {}",
                staged.canonical_sha1()
            ),
        )
        .into_response();
    }

    let content_descriptor = match staged.base64_json_descriptor(&[]).await {
        Ok(descriptor) => descriptor,
        Err(error) => return blob_store_error_response(&error),
    };
    let content_key = format!(
        "{FIELD_OVERFLOW_BLOB_PREFIX}{}.json",
        content_descriptor.sha256
    );
    if let Err(error) = blob_store
        .put_staged_base64_json(
            &content_key,
            &staged,
            &[],
            content_descriptor.serialized_len,
        )
        .await
    {
        return blob_store_error_response(&error);
    }

    let canonical_descriptor = match staged
        .base64_json_descriptor(canonical_prefix.as_bytes())
        .await
    {
        Ok(descriptor) => descriptor,
        Err(error) => return blob_store_error_response(&error),
    };
    let canonical_key = format!(
        "{FIELD_OVERFLOW_BLOB_PREFIX}{}.json",
        canonical_descriptor.sha256
    );
    if let Err(error) = blob_store
        .put_staged_base64_json(
            &canonical_key,
            &staged,
            canonical_prefix.as_bytes(),
            canonical_descriptor.serialized_len,
        )
        .await
    {
        return blob_store_error_response(&error);
    }

    let initial_fields = serde_json::json!({
        "Id": expected_object_id,
        "RepositoryId": repository_id,
        "Size": declared_len as i64,
        "Content": blob_ref_value(&content_key, content_descriptor.serialized_len),
        "CanonicalBytes": blob_ref_value(&canonical_key, canonical_descriptor.serialized_len),
        "Status": "Durable",
        "CreatedAt": now,
    });
    let final_guard = state.acquire_commons_write_guardrail_lock(&tenant).await;
    // Convert the pending reservation back into a final cap check while the
    // mutation lock prevents another writer from taking the released bytes.
    drop(storage_reservation.take());
    if let Err(error) = state
        .enforce_commons_storage_cap_for_write(
            &tenant,
            entity_type,
            &expected_object_id,
            "Create",
            &initial_fields,
        )
        .await
    {
        drop(final_guard);
        return storage_cap_error_response(error);
    }
    if let Err(response) = run_write_prechecks(
        &state,
        &tenant,
        entity_type,
        &expected_object_id,
        ("Create", "create"),
        &initial_fields,
        None,
    )
    .await
    {
        drop(final_guard);
        return response;
    }
    if let Err(response) =
        enforce_commons_account_verified_for_write(&state, &tenant, entity_type, &initial_fields)
            .await
    {
        drop(final_guard);
        return *response;
    }
    if let Err(response) = authorize_mutation(
        &state,
        &tenant,
        &security_ctx,
        &agent_ctx,
        CREATE_ACTION,
        MutationResource {
            entity_type,
            entity_id: &expected_object_id,
            attrs: &attrs,
        },
    )
    .await
    {
        drop(final_guard);
        return response;
    }

    let create_result = state
        .get_or_create_tenant_entity(&tenant, entity_type, &expected_object_id, initial_fields)
        .await;
    match create_result {
        Ok(response) => {
            let _ = agent_ctx;
            state.clear_commons_storage_projection_cache_for_entity(entity_type);
            drop(final_guard);
            let mut state_json = serde_json::to_value(&response.state).unwrap_or_default();
            remove_binary_fields_from_create_response(&mut state_json);
            let body = annotate_entity(
                state_json,
                format!("$metadata#{entity_type}s/$entity"),
                Some(format!("{entity_type}s('{expected_object_id}')")),
            );
            ODataResponse {
                status: StatusCode::CREATED,
                body,
            }
            .into_response()
        }
        Err(error) => {
            drop(final_guard);
            odata_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "EntityCreateFailed",
                &error.to_string(),
            )
            .into_response()
        }
    }
}

fn expected_object_id(headers: &HeaderMap) -> Result<String, Box<axum::response::Response>> {
    let Some(value) = headers.get(EXPECTED_OBJECT_ID_HEADER) else {
        return Err(Box::new(
            odata_error(
                StatusCode::PRECONDITION_REQUIRED,
                "MissingExpectedObjectId",
                "X-Expected-Object-Id header required for pre-body authorization",
            )
            .into_response(),
        ));
    };
    let object_id = value.to_str().map(str::trim).map_err(|_| {
        Box::new(
            odata_error(
                StatusCode::BAD_REQUEST,
                "InvalidExpectedObjectId",
                "X-Expected-Object-Id must be visible ASCII",
            )
            .into_response(),
        )
    })?;
    if object_id.len() != 40
        || !object_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(Box::new(
            odata_error(
                StatusCode::BAD_REQUEST,
                "InvalidExpectedObjectId",
                "X-Expected-Object-Id must be 40 lowercase hexadecimal characters",
            )
            .into_response(),
        ));
    }
    Ok(object_id.to_string())
}

#[cfg(test)]
mod tests;
