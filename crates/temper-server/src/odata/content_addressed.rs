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
use sha1::Digest;
use tokio_stream::StreamExt as _;

use super::account_verification::enforce_commons_account_verified_for_write;
use super::authz::{
    CREATE_ACTION, MutationResource, authorize_mutation, request_security_context,
    resource_attrs_from_body,
};
use super::common::{extract_tenant, run_write_prechecks};
use super::response::annotate_entity;
use super::storage_guardrails::enforce_commons_storage_cap;
use crate::identity::ResolvedIdentity;
use crate::request_context::extract_agent_context;
use crate::response::{ODataResponse, odata_error};
use crate::state::ServerState;

const MAX_OBJECT_BYTES: usize = 2 * 1024 * 1024 * 1024;

/// `POST /tdata/Blobs/Temper.IngestRaw` — stream raw blob bytes,
/// hash them, persist a `Blob` row keyed by the SHA-1 of the
/// canonical form `blob <len>\0<body>`.
///
/// Required headers:
///   * `Content-Length` — declared body length, used both for the
///     canonical hash prefix and as a defence against open-ended
///     streams.
///   * `X-Repository-Id` — foreign key back to the parent repo.
///
/// Optional:
///   * `X-Tenant-Id`, principal headers — same as any OData write.
pub async fn handle_blob_ingest_raw(
    State(state): State<ServerState>,
    resolved_id: Option<Extension<ResolvedIdentity>>,
    headers: HeaderMap,
    body: Body,
) -> impl IntoResponse {
    ingest_raw_inner(
        state,
        resolved_id.map(|Extension(identity)| identity),
        headers,
        body,
        "Blob",
        "blob",
    )
    .await
}

async fn ingest_raw_inner(
    state: ServerState,
    resolved_identity: Option<ResolvedIdentity>,
    headers: HeaderMap,
    body: Body,
    entity_type: &str,
    kind_tag: &str,
) -> axum::response::Response {
    let tenant = match extract_tenant(&headers, &state) {
        Ok(t) => t,
        Err(e) => return e.into_response(),
    };
    let mut agent_ctx = extract_agent_context(&headers);
    if let Some(ref identity) = resolved_identity {
        agent_ctx.agent_id = Some(identity.agent_instance_id.clone());
        agent_ctx.agent_type = Some(identity.agent_type_name.clone());
    }

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
    if declared_len > MAX_OBJECT_BYTES {
        return odata_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "ObjectTooLarge",
            &format!("declared size {declared_len} exceeds {MAX_OBJECT_BYTES}"),
        )
        .into_response();
    }

    // Hash the canonical bytes incrementally. We need the body
    // around to write into the row, but the hasher runs in
    // streaming fashion regardless — Vec<u8> is just the sink.
    let mut hasher = sha1::Sha1::new();
    let prefix = format!("{kind_tag} {declared_len}\0");
    hasher.update(prefix.as_bytes());

    let mut content = Vec::with_capacity(declared_len);
    let mut stream = body.into_data_stream();
    while let Some(chunk_result) = stream.next().await {
        let chunk = match chunk_result {
            Ok(c) => c,
            Err(e) => {
                return odata_error(
                    StatusCode::BAD_REQUEST,
                    "BodyStreamError",
                    &format!("body stream failed: {e}"),
                )
                .into_response();
            }
        };
        if content.len() + chunk.len() > declared_len {
            return odata_error(
                StatusCode::BAD_REQUEST,
                "BodyExceedsContentLength",
                "body bytes exceed declared Content-Length",
            )
            .into_response();
        }
        hasher.update(&chunk);
        content.extend_from_slice(&chunk);
    }
    if content.len() != declared_len {
        return odata_error(
            StatusCode::BAD_REQUEST,
            "BodyShorterThanContentLength",
            &format!("expected {declared_len} body bytes, got {}", content.len()),
        )
        .into_response();
    }

    let sha = hex_lower(&hasher.finalize());

    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD;
    let canonical_b64 = {
        let mut buf = Vec::with_capacity(prefix.len() + content.len());
        buf.extend_from_slice(prefix.as_bytes());
        buf.extend_from_slice(&content);
        b64.encode(&buf)
    };
    let content_b64 = b64.encode(&content);

    let now = chrono::Utc::now().to_rfc3339();
    let initial_fields = serde_json::json!({
        "Id": sha,
        "RepositoryId": repository_id,
        "Size": declared_len as i64,
        "Content": content_b64,
        "CanonicalBytes": canonical_b64,
        "Status": "Durable",
        "CreatedAt": now,
    });

    let _commons_guardrail_lock = state.acquire_commons_write_guardrail_lock(&tenant).await;

    if let Err(resp) = run_write_prechecks(
        &state,
        &tenant,
        entity_type,
        &sha,
        ("Create", "create"),
        &initial_fields,
        None,
    )
    .await
    {
        return resp;
    }

    if let Err(resp) =
        enforce_commons_account_verified_for_write(&state, &tenant, entity_type, &initial_fields)
            .await
    {
        return *resp;
    }

    if let Err(resp) = enforce_commons_storage_cap(
        &state,
        &tenant,
        entity_type,
        &sha,
        "Create",
        &initial_fields,
    )
    .await
    {
        return resp;
    }

    let security_ctx = request_security_context(&headers, &agent_ctx, resolved_identity.as_ref());
    let attrs = resource_attrs_from_body(&state, &tenant, entity_type, &sha, &initial_fields);
    if let Err(response) = authorize_mutation(
        &state,
        &tenant,
        &security_ctx,
        &agent_ctx,
        CREATE_ACTION,
        MutationResource {
            entity_type,
            entity_id: &sha,
            attrs: &attrs,
        },
    )
    .await
    {
        return response;
    }

    match state
        .get_or_create_tenant_entity(&tenant, entity_type, &sha, initial_fields)
        .await
    {
        Ok(response) => {
            let _ = agent_ctx;
            state.clear_commons_storage_projection_cache_for_entity(entity_type);
            let mut state_json = serde_json::to_value(&response.state).unwrap_or_default();
            crate::blobs::hydrate_blob_refs_for_tenant(&state, &tenant, &mut state_json).await;
            let body = annotate_entity(
                state_json,
                format!("$metadata#{entity_type}s/$entity"),
                Some(format!("{entity_type}s('{sha}')")),
            );
            ODataResponse {
                status: StatusCode::CREATED,
                body,
            }
            .into_response()
        }
        Err(e) => odata_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "EntityCreateFailed",
            &e.to_string(),
        )
        .into_response(),
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}
