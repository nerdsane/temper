//! Governed, bounded collection workflow observation (ADR-0181).

mod cursor;
mod types;

use std::collections::{BTreeMap, BTreeSet};

use axum::Extension;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use temper_authz::AuthenticatedRequestContext;
use temper_runtime::persistence::PersistenceError;

use self::types::{
    MemberPageResponse, MemberView, WorkflowDetailResponse, WorkflowListResponse, WorkflowSummary,
};
use crate::authz::{
    ResourceAuthorization, observe_tenant_scope, require_authenticated_context,
    require_resource_authorization,
};
use crate::state::ServerState;
use crate::storage::BoxedEventStore;
use crate::trigger::collection_workflow::{
    CollectionWorkflowRecordV1, CollectionWorkflowStatus, find_collection_intents,
    list_collection_workflow_ids_page, load_collection_record,
};
use crate::trigger::delivery::load_delivery_record;

const DEFAULT_WORKFLOW_LIMIT: usize = 50;
const MAX_WORKFLOW_LIMIT: usize = 100;
const WORKFLOW_SCAN_BUDGET: usize = 400;
const DEFAULT_MEMBER_LIMIT: usize = 64;
const MAX_MEMBER_LIMIT: usize = 64;
const VIEW_ACTION: &str = "ViewCollectionWorkflow";
const RESOURCE_TYPE: &str = "CollectionWorkflow";

#[derive(Debug, Deserialize)]
pub(crate) struct WorkflowListQuery {
    limit: Option<usize>,
    cursor: Option<String>,
    status: Option<CollectionWorkflowStatus>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct MemberListQuery {
    limit: Option<usize>,
    cursor: Option<String>,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: &'static str,
}

/// Stable sanitized error returned by collection Observe routes.
#[derive(Debug)]
pub(crate) struct ObserveError {
    status: StatusCode,
    category: &'static str,
}

impl ObserveError {
    const fn new(status: StatusCode, category: &'static str) -> Self {
        Self { status, category }
    }

    fn storage(error: PersistenceError) -> Self {
        let category = match error {
            PersistenceError::ConcurrencyViolation { .. } => "storage_conflict",
            PersistenceError::Serialization(_) => "corrupt_workflow_record",
            PersistenceError::Storage(_) => "storage_unavailable",
        };
        Self::new(StatusCode::SERVICE_UNAVAILABLE, category)
    }
}

impl IntoResponse for ObserveError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorBody {
                error: self.category,
            }),
        )
            .into_response()
    }
}

fn authenticated(
    context: Option<&AuthenticatedRequestContext>,
) -> Result<&AuthenticatedRequestContext, ObserveError> {
    require_authenticated_context(context)
        .map_err(|status| ObserveError::new(status, "authentication_required"))
}

fn valid_limit(
    supplied: Option<usize>,
    default: usize,
    maximum: usize,
) -> Result<usize, ObserveError> {
    let limit = supplied.unwrap_or(default);
    if limit == 0 || limit > maximum {
        return Err(ObserveError::new(StatusCode::BAD_REQUEST, "invalid_limit"));
    }
    Ok(limit)
}

fn authorize_record(
    state: &ServerState,
    authenticated: &AuthenticatedRequestContext,
    record: &CollectionWorkflowRecordV1,
) -> Result<(), StatusCode> {
    require_resource_authorization(
        state,
        authenticated,
        ResourceAuthorization {
            action: VIEW_ACTION,
            resource_type: RESOURCE_TYPE,
            resource_id: &record.workflow_id,
            resource_attrs: BTreeMap::from([
                (
                    "declaration".into(),
                    serde_json::json!(record.declaration_name),
                ),
                (
                    "source_entity_type".into(),
                    serde_json::json!(record.source_entity_type),
                ),
                ("status".into(), serde_json::json!(record.status)),
            ]),
        },
    )
}

async fn load_record(
    store: &BoxedEventStore,
    tenant: &str,
    workflow_id: &str,
) -> Result<CollectionWorkflowRecordV1, ObserveError> {
    load_collection_record(store, tenant, workflow_id)
        .await
        .map_err(ObserveError::storage)?
        .map(|(record, _)| record)
        .ok_or_else(|| ObserveError::new(StatusCode::NOT_FOUND, "workflow_not_found"))
}

struct WorkflowProjection {
    members: Vec<MemberView>,
    total_attempts: u32,
    oldest_active_age_ms: Option<u64>,
}

async fn project_members(
    store: &BoxedEventStore,
    record: &CollectionWorkflowRecordV1,
) -> Result<WorkflowProjection, ObserveError> {
    let active_ids = record
        .members
        .iter()
        .filter(|member| {
            member.status == crate::trigger::collection_workflow::CollectionMemberStatus::InFlight
        })
        .filter_map(|member| {
            member
                .cancellation_delivery_id
                .as_ref()
                .or(member.delivery_id.as_ref())
                .cloned()
        })
        .collect::<BTreeSet<_>>();
    let intents = find_collection_intents(store, record, &active_ids)
        .await
        .map_err(ObserveError::storage)?;
    let mut oldest = None;
    let mut members = Vec::with_capacity(record.members.len());
    for member in &record.members {
        let active_id = (member.status
            == crate::trigger::collection_workflow::CollectionMemberStatus::InFlight)
            .then(|| {
                member
                    .cancellation_delivery_id
                    .as_ref()
                    .or(member.delivery_id.as_ref())
            })
            .flatten();
        let Some(delivery_id) = active_id else {
            members.push(MemberView::from(member));
            continue;
        };
        let intent = intents.get(delivery_id).cloned().ok_or_else(|| {
            ObserveError::new(StatusCode::SERVICE_UNAVAILABLE, "corrupt_workflow_record")
        })?;
        let (delivery, _) = load_delivery_record(store, intent)
            .await
            .map_err(ObserveError::storage)?;
        let age = temper_runtime::scheduler::sim_now()
            .signed_duration_since(delivery.intent.created_at)
            .num_milliseconds()
            .max(0) as u64;
        oldest = Some(oldest.map_or(age, |current: u64| current.max(age)));
        let attempts = if member.cancellation_delivery_id.as_ref() == Some(delivery_id) {
            u32::from(member.attempts)
        } else {
            delivery.attempts
        };
        members.push(MemberView::from_delivery(member, &delivery, attempts));
    }
    let total_attempts = members.iter().map(MemberView::attempts).sum();
    Ok(WorkflowProjection {
        members,
        total_attempts,
        oldest_active_age_ms: oldest,
    })
}

/// GET `/observe/collection-workflows` — bounded, authorized workflow summaries.
pub(crate) async fn handle_list_workflows(
    State(state): State<ServerState>,
    authenticated_context: Option<Extension<AuthenticatedRequestContext>>,
    Query(query): Query<WorkflowListQuery>,
) -> Result<Json<WorkflowListResponse>, ObserveError> {
    let authenticated = authenticated(authenticated_context.as_deref())?;
    let tenant = observe_tenant_scope(authenticated).as_str();
    let limit = valid_limit(query.limit, DEFAULT_WORKFLOW_LIMIT, MAX_WORKFLOW_LIMIT)?;
    let after = query
        .cursor
        .as_deref()
        .map(|value| cursor::decode_workflow(value, tenant))
        .transpose()
        .map_err(|()| ObserveError::new(StatusCode::BAD_REQUEST, "invalid_cursor"))?;
    let (store, _) = state
        .event_journal()
        .ok_or_else(|| ObserveError::new(StatusCode::SERVICE_UNAVAILABLE, "storage_unavailable"))?;
    let ids = list_collection_workflow_ids_page(
        &store,
        tenant,
        after.as_deref(),
        WORKFLOW_SCAN_BUDGET + 1,
    )
    .await
    .map_err(ObserveError::storage)?;

    let mut value = Vec::new();
    let mut last_inspected = None;
    let mut inspected = 0_usize;
    for workflow_id in ids.iter().take(WORKFLOW_SCAN_BUDGET) {
        inspected += 1;
        last_inspected = Some(workflow_id.as_str());
        let record = load_record(&store, tenant, workflow_id).await?;
        if query.status.is_some_and(|status| record.status != status) {
            continue;
        }
        if authorize_record(&state, authenticated, &record).is_err() {
            continue;
        }
        let projection = project_members(&store, &record).await?;
        value.push(WorkflowSummary::from_record(
            &record,
            projection.total_attempts,
            projection.oldest_active_age_ms,
        ));
        if value.len() == limit {
            break;
        }
    }
    let has_more = inspected < ids.len();
    let next_cursor = (has_more && last_inspected.is_some())
        .then(|| cursor::encode_workflow(tenant, last_inspected.expect("checked above")));
    Ok(Json(WorkflowListResponse { value, next_cursor }))
}

/// GET `/observe/collection-workflows/{workflow_id}` — one redacted workflow.
pub(crate) async fn handle_get_workflow(
    State(state): State<ServerState>,
    authenticated_context: Option<Extension<AuthenticatedRequestContext>>,
    Path(workflow_id): Path<String>,
) -> Result<Json<WorkflowDetailResponse>, ObserveError> {
    let authenticated = authenticated(authenticated_context.as_deref())?;
    let tenant = observe_tenant_scope(authenticated).as_str();
    let (store, _) = state
        .event_journal()
        .ok_or_else(|| ObserveError::new(StatusCode::SERVICE_UNAVAILABLE, "storage_unavailable"))?;
    let record = load_record(&store, tenant, &workflow_id).await?;
    authorize_record(&state, authenticated, &record)
        .map_err(|_| ObserveError::new(StatusCode::FORBIDDEN, "workflow_forbidden"))?;
    let projection = project_members(&store, &record).await?;
    Ok(Json(WorkflowDetailResponse {
        summary: WorkflowSummary::from_record(
            &record,
            projection.total_attempts,
            projection.oldest_active_age_ms,
        ),
        members: projection.members,
    }))
}

/// GET `/observe/collection-workflows/{workflow_id}/members` — redacted member page.
pub(crate) async fn handle_list_members(
    State(state): State<ServerState>,
    authenticated_context: Option<Extension<AuthenticatedRequestContext>>,
    Path(workflow_id): Path<String>,
    Query(query): Query<MemberListQuery>,
) -> Result<Json<MemberPageResponse>, ObserveError> {
    let authenticated = authenticated(authenticated_context.as_deref())?;
    let tenant = observe_tenant_scope(authenticated).as_str();
    let limit = valid_limit(query.limit, DEFAULT_MEMBER_LIMIT, MAX_MEMBER_LIMIT)?;
    let after = query
        .cursor
        .as_deref()
        .map(|value| cursor::decode_member(value, tenant, &workflow_id))
        .transpose()
        .map_err(|()| ObserveError::new(StatusCode::BAD_REQUEST, "invalid_cursor"))?;
    let (store, _) = state
        .event_journal()
        .ok_or_else(|| ObserveError::new(StatusCode::SERVICE_UNAVAILABLE, "storage_unavailable"))?;
    let record = load_record(&store, tenant, &workflow_id).await?;
    authorize_record(&state, authenticated, &record)
        .map_err(|_| ObserveError::new(StatusCode::FORBIDDEN, "workflow_forbidden"))?;
    let candidates = record
        .members
        .iter()
        .filter(|member| after.is_none_or(|index| member.member_index > index))
        .collect::<Vec<_>>();
    let projection = project_members(&store, &record).await?;
    let value = projection
        .members
        .into_iter()
        .filter(|member| after.is_none_or(|index| member.member_index() > index))
        .take(limit)
        .collect::<Vec<_>>();
    let next_cursor = (candidates.len() > value.len())
        .then(|| candidates[value.len() - 1].member_index)
        .map(|member_index| cursor::encode_member(tenant, &workflow_id, member_index));
    Ok(Json(MemberPageResponse { value, next_cursor }))
}

#[cfg(test)]
mod tests;
