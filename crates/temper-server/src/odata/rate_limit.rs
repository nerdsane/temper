use axum::http::{HeaderValue, StatusCode};
use axum::response::IntoResponse;
use temper_authz::SecurityContext;
use temper_runtime::tenant::TenantId;

use crate::response::odata_error;
use crate::state::ServerState;
use crate::state::account_verification::commons_subject;

/// Spend one commons write token from the authenticated caller's own bucket.
/// Owner fields in the request or the row do not choose the bucket.
pub(super) async fn enforce_commons_write_rate_limit(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    security_context: &SecurityContext,
) -> Result<(), axum::response::Response> {
    if !state.commons_guardrails_enabled(tenant) || rate_limit_exempt_entity(entity_type) {
        return Ok(());
    }

    let owner_id = commons_subject(security_context);
    if owner_id.trim().is_empty() || owner_id == "anonymous" {
        return Ok(());
    }

    match state
        .consume_commons_rate_limit_token(tenant, &owner_id, state.commons_write_action_class())
        .await
    {
        Ok(()) => Ok(()),
        Err(crate::state::rate_limit::CommonsRateLimitError::Exceeded(exceeded)) => {
            let mut response = odata_error(
                StatusCode::TOO_MANY_REQUESTS,
                "RateLimitExceeded",
                &format!(
                    "Rate limit exceeded for owner '{}' action class '{}'",
                    exceeded.owner_id, exceeded.action_class
                ),
            )
            .into_response();
            if let Some(retry_after) = exceeded.retry_after_secs
                && let Ok(value) = HeaderValue::from_str(&retry_after.to_string())
            {
                response.headers_mut().insert("Retry-After", value);
            }
            Err(response)
        }
        Err(err) => Err(odata_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "RateLimitError",
            &err.to_string(),
        )
        .into_response()),
    }
}

fn rate_limit_exempt_entity(entity_type: &str) -> bool {
    matches!(entity_type, "Owner" | "RateLimit")
}
