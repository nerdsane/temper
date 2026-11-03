//! `GET Set('key')/Property`: one structural property of one record, whatever
//! its size.
//!
//! A whole-record read inlines a field kept out of line only within its
//! hydration budget and leaves a larger one as its bare reference, which a
//! reader cannot follow on its own. This read answers that one field as
//! `{"@odata.context": ..., "value": ...}`, streaming an out-of-line value from
//! the blob store and checking it against its content address as it goes, so
//! the server never holds the field in memory and no size is refused.

use axum::body::Body;
use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::{StreamExt as _, stream};
use temper_authz::SecurityContext;
use temper_runtime::tenant::TenantId;

use super::authz::{READ_ACTION, authorize_read};
use super::read::{load_existing_entity_descriptor_body, load_pg_entity_descriptor_body};
use crate::blob_store::BlobStreamRead;
use crate::blobs::{
    BlobHydrationBudget, field_overflow_descriptor, hydrate_blob_refs_for_tenant_with_budget,
};
use crate::query_eval::find_nav_target;
use crate::response::{ODataResponse, odata_error};
use crate::state::ServerState;

/// Whether `property` is a navigation property of `entity_type`, in the
/// tenant's schema or the server's own.
pub(super) fn is_navigation_property(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    property: &str,
) -> bool {
    // Only a lookup of the schema: a lock a writer panicked under still answers it.
    let registry = state
        .registry
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    registry
        .get_tenant(tenant)
        .and_then(|config| find_nav_target(&config.csdl, entity_type, property))
        .or_else(|| find_nav_target(&state.csdl, entity_type, property))
        .is_some()
}

/// Answer one field of `set_name('key')`, authorized as a read of the record.
pub(super) async fn handle_property_read(
    state: &ServerState,
    tenant: &TenantId,
    security_ctx: &SecurityContext,
    entity_type: &str,
    set_name: &str,
    key: &str,
    property: &str,
) -> Response {
    // Authorization precedes any blob-store read, as for a whole record.
    let loaded = if state.is_pg_actor_backed(tenant, entity_type) && state.pg_actor_system.is_some()
    {
        load_pg_entity_descriptor_body(state, tenant, entity_type, set_name, key).await
    } else {
        load_existing_entity_descriptor_body(state, tenant, entity_type, set_name, key).await
    };
    let body = match loaded {
        Ok(body) => body,
        Err(response) => return response,
    };
    if let Err(response) = authorize_read(
        state,
        tenant,
        security_ctx,
        READ_ACTION,
        entity_type,
        key,
        &body,
    ) {
        return *response;
    }

    let Some(field) = body.get("fields").and_then(|fields| fields.get(property)) else {
        return odata_error(
            StatusCode::NOT_FOUND,
            "PropertyNotFound",
            &format!("{set_name}('{key}') has no property '{property}'"),
        )
        .into_response();
    };
    let context = format!("$metadata#{set_name}('{key}')/{property}");

    let Some(descriptor) = field_overflow_descriptor(field) else {
        // Inline: a value inside it may still be out of line, so it reads as
        // the whole record's fields do.
        let mut value = field.clone();
        hydrate_blob_refs_for_tenant_with_budget(
            state,
            tenant,
            &mut value,
            &BlobHydrationBudget::generic_response(),
        )
        .await;
        return ODataResponse {
            status: StatusCode::OK,
            body: serde_json::json!({"@odata.context": context, "value": value}),
        }
        .into_response();
    };

    let stored = match state
        .stream_blob_object(tenant, descriptor.key, descriptor.serialized_bytes)
        .await
    {
        Ok(BlobStreamRead::Found(stored))
            if stored.content_length() == descriptor.serialized_bytes =>
        {
            stored
        }
        Ok(BlobStreamRead::Found(_) | BlobStreamRead::TooLarge { .. }) => {
            return odata_error(
                StatusCode::CONFLICT,
                "PropertyValueSizeMismatch",
                &format!(
                    "{set_name}('{key}').{property}'s stored value is not the {} bytes its reference names",
                    descriptor.serialized_bytes
                ),
            )
            .into_response();
        }
        Ok(BlobStreamRead::Missing) => {
            return odata_error(
                StatusCode::NOT_FOUND,
                "PropertyValueMissing",
                &format!(
                    "{set_name}('{key}').{property}'s stored value '{}' was not found",
                    descriptor.key
                ),
            )
            .into_response();
        }
        Err(error) => {
            tracing::error!(%error, blob_key = %descriptor.key, "property value read failed");
            return odata_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "PropertyValueUnavailable",
                &format!("{set_name}('{key}').{property} is unavailable for now"),
            )
            .into_response();
        }
    };

    // The stored object is the field's JSON, so it goes between the two
    // halves of the envelope as it is.
    let head = serde_json::json!({ "@odata.context": context }).to_string();
    let head = Bytes::from(format!("{},\"value\":", &head[..head.len() - 1]));
    let tail = Bytes::from_static(b"}");
    let length = head.len() as u64 + descriptor.serialized_bytes + tail.len() as u64;
    let value = stored.verify_sha256(descriptor.sha256).into_stream();
    let envelope = stream::once(async move { Ok(head) })
        .chain(value)
        .chain(stream::once(async move { Ok(tail) }));

    let mut response = Response::new(Body::from_stream(envelope));
    let headers = response.headers_mut();
    headers.insert("OData-Version", HeaderValue::from_static("4.0"));
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/json;odata.metadata=minimal"),
    );
    headers.insert(CONTENT_LENGTH, HeaderValue::from(length));
    response
}
