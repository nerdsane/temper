//! Authenticated binary responses for Blob primitive values.

use axum::body::Body;
use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE, ETAG};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use temper_authz::SecurityContext;
use temper_runtime::tenant::TenantId;

use super::authz::{READ_ACTION, authorize_read};
use super::common::resolve_entity_type;
use super::read::{entity_set_not_found_response, load_existing_entity_descriptor_body};
use crate::blobs::field_overflow_descriptor;
use crate::response::odata_error;
use crate::state::ServerState;

const MAX_INLINE_BLOB_MEDIA_BYTES: u64 = 128 * 1024;

pub(super) async fn handle_blob_primitive_stream(
    state: &ServerState,
    tenant: &TenantId,
    security_ctx: &SecurityContext,
    set_name: &str,
    key: &str,
    property: &str,
) -> Response {
    let entity_type = match resolve_entity_type(state, tenant, set_name) {
        Some(entity_type) if entity_type == "Blob" => entity_type,
        Some(_) => {
            return odata_error(
                StatusCode::BAD_REQUEST,
                "UnsupportedPrimitiveStream",
                "primitive $value streaming is supported only for Blob binary fields",
            )
            .into_response();
        }
        None => return entity_set_not_found_response(state, tenant, set_name).await,
    };
    if !matches!(property, "Content" | "CanonicalBytes") {
        return odata_error(
            StatusCode::BAD_REQUEST,
            "UnsupportedBlobProperty",
            "Blob primitive $value supports only Content and CanonicalBytes",
        )
        .into_response();
    }

    let body = match load_existing_entity_descriptor_body(
        state,
        tenant,
        &entity_type,
        set_name,
        key,
    )
    .await
    {
        Ok(body) => body,
        Err(response) => return response,
    };
    if let Err(response) = authorize_read(
        state,
        tenant,
        security_ctx,
        READ_ACTION,
        &entity_type,
        key,
        &body,
    ) {
        return *response;
    }

    let fields = body.get("fields").unwrap_or(&body);
    let Some(raw_size) = fields.get("Size").and_then(serde_json::Value::as_u64) else {
        return odata_error(
            StatusCode::CONFLICT,
            "InvalidBlobMetadata",
            "Blob.Size is missing or invalid",
        )
        .into_response();
    };
    let Some(field) = fields.get(property) else {
        return odata_error(
            StatusCode::NOT_FOUND,
            "BlobPropertyNotFound",
            &format!("Blob('{key}').{property} is not available"),
        )
        .into_response();
    };
    let decoded_size = if property == "Content" {
        raw_size
    } else {
        let prefix_len = format!("blob {raw_size}\0").len() as u64;
        match raw_size.checked_add(prefix_len) {
            Some(size) => size,
            None => {
                return odata_error(
                    StatusCode::CONFLICT,
                    "InvalidBlobMetadata",
                    "Blob canonical size overflowed u64",
                )
                .into_response();
            }
        }
    };
    let Some(expected_encoded_size) = decoded_size
        .checked_add(2)
        .map(|bytes| bytes / 3)
        .and_then(|groups| groups.checked_mul(4))
        .and_then(|base64_bytes| base64_bytes.checked_add(2))
    else {
        return odata_error(
            StatusCode::CONFLICT,
            "InvalidBlobMetadata",
            "Blob encoded size overflowed u64",
        )
        .into_response();
    };

    if let Some(inline) = field.as_str() {
        if decoded_size > MAX_INLINE_BLOB_MEDIA_BYTES {
            return odata_error(
                StatusCode::CONFLICT,
                "InlineBlobMediaTooLarge",
                "large Blob media must use a field-overflow descriptor",
            )
            .into_response();
        }
        if inline.len() as u64 + 2 != expected_encoded_size {
            return odata_error(
                StatusCode::CONFLICT,
                "BlobMediaSizeMismatch",
                "inline Blob media length does not match Blob.Size",
            )
            .into_response();
        }
        let decoded = match base64::engine::general_purpose::STANDARD.decode(inline) {
            Ok(decoded) if decoded.len() as u64 == decoded_size => decoded,
            Ok(_) => {
                return odata_error(
                    StatusCode::CONFLICT,
                    "BlobMediaSizeMismatch",
                    "decoded inline Blob media length does not match Blob.Size",
                )
                .into_response();
            }
            Err(error) => {
                return odata_error(
                    StatusCode::CONFLICT,
                    "InvalidBlobMedia",
                    &format!("inline Blob media is not valid base64: {error}"),
                )
                .into_response();
            }
        };
        return media_response(Body::from(decoded), decoded_size, key);
    }

    let Some(descriptor) = field_overflow_descriptor(field) else {
        return odata_error(
            StatusCode::CONFLICT,
            "InvalidBlobDescriptor",
            &format!("Blob('{key}').{property} has an invalid media descriptor"),
        )
        .into_response();
    };
    let blob_key = descriptor.key;
    let encoded_size = descriptor.serialized_bytes;
    if encoded_size != expected_encoded_size {
        return odata_error(
            StatusCode::CONFLICT,
            "BlobMediaSizeMismatch",
            "Blob media descriptor length does not match the decoded field size",
        )
        .into_response();
    }

    let encoded = match state
        .stream_blob_object(tenant, blob_key, encoded_size)
        .await
    {
        Ok(crate::blob_store::BlobStreamRead::Found(stream)) => stream,
        Ok(crate::blob_store::BlobStreamRead::Missing) => {
            return odata_error(
                StatusCode::NOT_FOUND,
                "BlobMediaMissing",
                &format!("Blob media object '{blob_key}' was not found"),
            )
            .into_response();
        }
        Ok(crate::blob_store::BlobStreamRead::TooLarge { .. }) => {
            return odata_error(
                StatusCode::CONFLICT,
                "BlobMediaSizeMismatch",
                "Blob media object exceeds its descriptor size",
            )
            .into_response();
        }
        Err(error) => {
            tracing::error!(%error, %blob_key, "Blob media object-store read failed");
            return odata_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "BlobMediaUnavailable",
                "Blob media is temporarily unavailable",
            )
            .into_response();
        }
    };
    if encoded.content_length() != encoded_size {
        return odata_error(
            StatusCode::CONFLICT,
            "BlobMediaSizeMismatch",
            "Blob media object length does not match its descriptor",
        )
        .into_response();
    }
    let encoded = encoded.verify_sha256(descriptor.sha256);
    let decoded = crate::blob_store::decode_json_base64_stream(encoded, decoded_size);
    media_response(Body::from_stream(decoded.into_stream()), decoded_size, key)
}

fn media_response(body: Body, decoded_size: u64, key: &str) -> Response {
    let mut response = Response::new(body);
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    if let Ok(value) = HeaderValue::from_str(&decoded_size.to_string()) {
        response.headers_mut().insert(CONTENT_LENGTH, value);
    }
    if let Ok(value) = HeaderValue::from_str(&format!("\"{key}\"")) {
        response.headers_mut().insert(ETAG, value);
    }
    response
}
