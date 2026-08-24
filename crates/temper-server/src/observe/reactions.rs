//! Read-only durable reaction delivery observation (ADR-0158).

use axum::Extension;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use temper_authz::AuthenticatedRequestContext;

use crate::authz::{observe_tenant_scope, require_authenticated_context, require_observe_auth};
use crate::state::ServerState;
use crate::trigger::delivery::{
    DeliveryKind, ReactionDeliveryRecord, ReactionDeliveryStatus, delivery_journal_id,
    find_delivery_record, list_delivery_records_page,
};

const DEFAULT_LIST_LIMIT: usize = 100;
const MAX_LIST_LIMIT: usize = 1_000;

#[derive(Debug, Deserialize)]
pub(crate) struct ReactionListQuery {
    limit: Option<usize>,
    status: Option<ReactionDeliveryStatus>,
    after: Option<String>,
}

#[derive(Debug, Serialize)]
struct ReactionDeliveryView {
    kind: DeliveryKind,
    delivery_id: String,
    root_delivery_id: String,
    tenant: String,
    source_entity_type: String,
    source_entity_id: String,
    source_action: String,
    source_sequence: u64,
    target_entity_id: Option<String>,
    trigger_name: String,
    depth: u32,
    status: ReactionDeliveryStatus,
    attempts: u32,
    manual_retries: u32,
    fencing_token: u64,
    lease_expires_at: Option<chrono::DateTime<chrono::Utc>>,
    next_attempt_at: Option<chrono::DateTime<chrono::Utc>>,
    deadline: Option<chrono::DateTime<chrono::Utc>>,
    schema_digest: Option<String>,
    transient_failure: bool,
    last_error: Option<String>,
    principal_id: Option<String>,
    principal_kind: Option<String>,
}

#[derive(Debug, Serialize)]
struct ReactionAttemptView {
    sequence: u64,
    status: ReactionDeliveryStatus,
    attempts: u32,
    manual_retries: u32,
    fencing_token: u64,
    recorded_at: chrono::DateTime<chrono::Utc>,
}

impl From<&ReactionDeliveryRecord> for ReactionDeliveryView {
    fn from(record: &ReactionDeliveryRecord) -> Self {
        let principal = record.intent.authority.get("principal");
        Self {
            kind: record.intent.kind,
            delivery_id: record.intent.delivery_id.clone(),
            root_delivery_id: record.intent.root_delivery_id.clone(),
            tenant: record.intent.tenant.clone(),
            source_entity_type: record.intent.source_entity_type.clone(),
            source_entity_id: record.intent.source_entity_id.clone(),
            source_action: record.intent.source_action.clone(),
            source_sequence: record.intent.source_sequence,
            target_entity_id: record.intent.target_entity_id.clone(),
            trigger_name: record.intent.trigger_name.clone(),
            depth: record.intent.depth,
            status: record.status,
            attempts: record.attempts,
            manual_retries: record.manual_retries,
            fencing_token: record.fencing_token,
            lease_expires_at: record.lease_expires_at,
            next_attempt_at: record.next_attempt_at,
            deadline: record.intent.not_before,
            schema_digest: record
                .intent
                .state_timeout
                .as_ref()
                .map(|clock| clock.schema_digest.clone()),
            transient_failure: record.transient_failure,
            last_error: record.last_error.clone(),
            principal_id: principal
                .and_then(|value| value.get("id"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            principal_kind: principal
                .and_then(|value| value.get("kind"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
        }
    }
}

/// GET `/observe/reactions` — bounded tenant-scoped delivery list.
pub(crate) async fn handle_list_reactions(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    Query(query): Query<ReactionListQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let authenticated = require_authenticated_context(authenticated.as_deref())?;
    require_observe_auth(&state, authenticated, "read_reactions", "ReactionDelivery")?;
    let tenant = observe_tenant_scope(authenticated);
    let (store, _) = state
        .event_journal()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let limit = query
        .limit
        .unwrap_or(DEFAULT_LIST_LIMIT)
        .clamp(1, MAX_LIST_LIMIT);
    let records = list_delivery_records_page(
        &store,
        tenant.as_str(),
        query.after.as_deref(),
        MAX_LIST_LIMIT,
    )
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let mut value = Vec::new();
    let mut last_inspected = None;
    for (record, _) in &records {
        last_inspected = Some(record.intent.delivery_id.clone());
        if query.status.is_none_or(|status| record.status == status) {
            value.push(ReactionDeliveryView::from(record));
            if value.len() == limit {
                break;
            }
        }
    }
    let next = (last_inspected.is_some()
        && (value.len() == limit || records.len() == MAX_LIST_LIMIT))
        .then_some(last_inspected)
        .flatten();
    Ok(Json(serde_json::json!({
        "total": value.len(),
        "value": value,
        "next": next,
    })))
}

/// GET `/observe/reactions/{delivery_id}` — redacted lifecycle detail.
pub(crate) async fn handle_get_reaction(
    State(state): State<ServerState>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
    Path(delivery_id): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let authenticated = require_authenticated_context(authenticated.as_deref())?;
    require_observe_auth(&state, authenticated, "read_reactions", "ReactionDelivery")?;
    let tenant = observe_tenant_scope(authenticated);
    let (store, _) = state
        .event_journal()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let Some((record, _)) = find_delivery_record(&store, tenant.as_str(), &delivery_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    else {
        return Err(StatusCode::NOT_FOUND);
    };
    let events = store
        .read_events(&delivery_journal_id(&record.intent), 0)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let attempts = events
        .into_iter()
        .filter_map(|event| {
            serde_json::from_value::<ReactionDeliveryRecord>(event.payload)
                .ok()
                .map(|snapshot| ReactionAttemptView {
                    sequence: event.sequence_nr,
                    status: snapshot.status,
                    attempts: snapshot.attempts,
                    manual_retries: snapshot.manual_retries,
                    fencing_token: snapshot.fencing_token,
                    recorded_at: event.metadata.timestamp,
                })
        })
        .collect::<Vec<_>>();
    Ok(Json(serde_json::json!({
        "delivery": ReactionDeliveryView::from(&record),
        "history": attempts,
    })))
}
