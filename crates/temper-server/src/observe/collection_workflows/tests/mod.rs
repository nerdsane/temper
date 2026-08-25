use axum::Extension;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use temper_authz::{AuthenticatedRequestContext, Principal, PrincipalKind, SecurityContext};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_store_turso::TursoEventStore;

use super::*;
use crate::registry::SpecRegistry;
use crate::storage::StorageStack;
use crate::trigger::collection_workflow::{
    CollectionExecutionActions, CollectionMemberReceipt, CollectionRequestedOutcome,
    CollectionWorkflowBudgets, CollectionWorkflowRecordV1, CollectionWorkflowStart, activate_start,
    append_collection_record_idempotent, recover_progress, workflow_append,
};
use crate::trigger::delivery::{
    ReactionDeliveryStatus, append_delivery_record, attach_intents, load_delivery_record,
};

mod progress;

fn context(tenant: &str, principal_id: &str) -> Extension<AuthenticatedRequestContext> {
    Extension(AuthenticatedRequestContext::new(
        TenantId::new(tenant),
        SecurityContext {
            principal: Principal {
                id: principal_id.to_string(),
                kind: PrincipalKind::Customer,
                role: None,
                acting_for: None,
                agent_type: None,
                attributes: Default::default(),
            },
            context_attrs: Default::default(),
            correlation_id: format!("collection-observe-{principal_id}"),
        },
    ))
}

async fn state() -> (ServerState, TursoEventStore, tempfile::TempDir) {
    let temp = tempfile::tempdir().expect("temporary directory");
    let url = format!("file:{}", temp.path().join("observe.db").display());
    let store = TursoEventStore::new(&url, None)
        .await
        .expect("durable Turso store");
    let mut state =
        ServerState::from_registry(ActorSystem::new("collection-observe"), SpecRegistry::new());
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    (state, store, temp)
}

fn workflow(tenant: &str, source_id: &str, roster: &[&str]) -> CollectionWorkflowRecordV1 {
    CollectionWorkflowRecordV1::start(CollectionWorkflowStart {
        tenant: tenant.into(),
        source_entity_type: "Batch".into(),
        source_entity_id: source_id.into(),
        declaration_name: "run_checks".into(),
        source_action: "StartChecks".into(),
        source_sequence: 1,
        schema_digest: "schema-v1".into(),
        schema_pin: None,
        authority: serde_json::json!({"principal": {"id": "secret-principal"}}),
        roster: roster.iter().map(|value| (*value).to_string()).collect(),
        budgets: CollectionWorkflowBudgets {
            max_members: roster.len() as u16,
            max_concurrency: 1,
            max_attempts: 3,
        },
    })
    .expect("valid workflow")
    .1
}

async fn seed(state: &ServerState, record: &CollectionWorkflowRecordV1) {
    let (store, _) = state.event_journal().expect("event journal");
    append_collection_record_idempotent(&store, 0, "CollectionWorkflow::StartedV1", record)
        .await
        .expect("seed workflow");
}

fn actions() -> CollectionExecutionActions<'static> {
    CollectionExecutionActions {
        member_entity: "CheckRun",
        member_action: "Start",
        member_cancel_action: "Cancel",
        timeout_action: "TimeoutChecks",
        on_success: "ChecksSucceeded",
        on_partial_failure: "ChecksPartiallyFailed",
        on_failure: "ChecksFailed",
        on_cancelled: "ChecksCancelled",
        on_timed_out: "ChecksTimedOut",
    }
}

async fn seed_activated(
    state: &ServerState,
    record: &mut CollectionWorkflowRecordV1,
) -> crate::trigger::delivery::PersistedReactionIntent {
    let (store, _) = state.event_journal().expect("event journal");
    let intents = activate_start(record, 0, &actions()).expect("activate initial window");
    let mut append =
        workflow_append(record, 0, "CollectionWorkflow::StartedV1").expect("workflow append");
    attach_intents(&mut append.events[0].payload, &intents).expect("attach member intents");
    store
        .append_batch(&[append])
        .await
        .expect("seed activated workflow");
    intents.into_iter().next().expect("one admitted member")
}

fn permit_reader(state: &ServerState, tenant: &str) {
    state
        .authz
        .reload_tenant_policies(
            tenant,
            r#"
permit(
  principal == Customer::"reader",
  action == Action::"ViewCollectionWorkflow",
  resource
);
"#,
        )
        .expect("valid collection observe policy");
}

fn permit_one_workflow(state: &ServerState, tenant: &str, workflow_id: &str) {
    state
        .authz
        .reload_tenant_policies(
            tenant,
            &format!(
                r#"
permit(
  principal == Customer::"reader",
  action == Action::"ViewCollectionWorkflow",
  resource == CollectionWorkflow::"{workflow_id}"
);
"#
            ),
        )
        .expect("valid resource-specific policy");
}

#[test]
fn limits_are_rejected_instead_of_clamped() {
    assert_eq!(valid_limit(None, 50, 100).expect("default"), 50);
    assert!(valid_limit(Some(0), 50, 100).is_err());
    assert!(valid_limit(Some(101), 50, 100).is_err());
}

#[tokio::test]
async fn list_and_member_pages_are_tenant_bound_paginated_and_redacted() {
    let (state, _store, _temp) = state().await;
    permit_reader(&state, "tenant-a");
    let first = workflow("tenant-a", "batch-a", &["secret-a", "secret-b"]);
    let second = workflow("tenant-a", "batch-b", &["secret-c"]);
    let foreign = workflow("tenant-b", "batch-foreign", &["foreign-secret"]);
    seed(&state, &first).await;
    seed(&state, &second).await;
    seed(&state, &foreign).await;

    let page = handle_list_workflows(
        State(state.clone()),
        Some(context("tenant-a", "reader")),
        Query(WorkflowListQuery {
            limit: Some(1),
            cursor: None,
            status: Some(CollectionWorkflowStatus::Running),
        }),
    )
    .await
    .expect("authorized list");
    let page = serde_json::to_value(page.0).expect("serialize page");
    assert_eq!(page["value"].as_array().map(Vec::len), Some(1));
    let cursor = page["next_cursor"].as_str().expect("continuation");
    let encoded = page.to_string();
    for private in [
        "secret-a",
        "secret-b",
        "secret-c",
        "foreign-secret",
        "authority",
    ] {
        assert!(!encoded.contains(private), "list exposed {private}");
    }

    let next = handle_list_workflows(
        State(state.clone()),
        Some(context("tenant-a", "reader")),
        Query(WorkflowListQuery {
            limit: Some(1),
            cursor: Some(cursor.to_string()),
            status: Some(CollectionWorkflowStatus::Running),
        }),
    )
    .await
    .expect("second page");
    let next = serde_json::to_value(next.0).expect("serialize page");
    assert_eq!(next["value"].as_array().map(Vec::len), Some(1));
    assert_ne!(
        page["value"][0]["workflow_id"],
        next["value"][0]["workflow_id"]
    );
    assert!(next["next_cursor"].is_null());

    let members = handle_list_members(
        State(state),
        Some(context("tenant-a", "reader")),
        Path(first.workflow_id.clone()),
        Query(MemberListQuery {
            limit: Some(1),
            cursor: None,
        }),
    )
    .await
    .expect("member page");
    let members = serde_json::to_value(members.0).expect("serialize member page");
    assert_eq!(members["value"].as_array().map(Vec::len), Some(1));
    assert!(members["next_cursor"].is_string());
    assert!(!members.to_string().contains("secret-a"));
}

#[tokio::test]
async fn detail_denial_is_forbidden_while_denied_list_rows_are_omitted() {
    let (state, _store, _temp) = state().await;
    permit_reader(&state, "tenant-a");
    let record = workflow("tenant-a", "batch-a", &["secret"]);
    seed(&state, &record).await;

    let denied_detail = handle_get_workflow(
        State(state.clone()),
        Some(context("tenant-a", "denied")),
        Path(record.workflow_id.clone()),
    )
    .await
    .expect_err("detail must be denied");
    assert_eq!(denied_detail.status, StatusCode::FORBIDDEN);

    let denied_list = handle_list_workflows(
        State(state),
        Some(context("tenant-a", "denied")),
        Query(WorkflowListQuery {
            limit: None,
            cursor: None,
            status: None,
        }),
    )
    .await
    .expect("denied rows are omitted");
    let denied_list = serde_json::to_value(denied_list.0).expect("serialize list");
    assert_eq!(denied_list["value"].as_array().map(Vec::len), Some(0));
}

#[tokio::test]
async fn list_applies_cedar_per_workflow_before_materializing_rows() {
    let (state, _store, _temp) = state().await;
    let visible = workflow("tenant-a", "batch-visible", &["visible-secret"]);
    let hidden = workflow("tenant-a", "batch-hidden", &["hidden-secret"]);
    seed(&state, &visible).await;
    seed(&state, &hidden).await;
    permit_one_workflow(&state, "tenant-a", &visible.workflow_id);

    let list = handle_list_workflows(
        State(state.clone()),
        Some(context("tenant-a", "reader")),
        Query(WorkflowListQuery {
            limit: None,
            cursor: None,
            status: None,
        }),
    )
    .await
    .expect("authorized subset");
    let list = serde_json::to_value(list.0).expect("serialize list");
    assert_eq!(list["value"].as_array().map(Vec::len), Some(1));
    assert_eq!(list["value"][0]["workflow_id"], visible.workflow_id);
    assert!(!list.to_string().contains("hidden-secret"));

    let denied = handle_get_workflow(
        State(state),
        Some(context("tenant-a", "reader")),
        Path(hidden.workflow_id),
    )
    .await
    .expect_err("hidden workflow detail");
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn denied_list_scan_stops_at_budget_and_returns_a_continuation() {
    let (state, _store, _temp) = state().await;
    let (store, _) = state.event_journal().expect("event journal");
    let mut appends = Vec::with_capacity(WORKFLOW_SCAN_BUDGET + 1);
    for index in 0..=WORKFLOW_SCAN_BUDGET {
        let record = workflow("tenant-a", &format!("batch-{index:03}"), &["private"]);
        appends.push(
            workflow_append(&record, 0, "CollectionWorkflow::StartedV1").expect("workflow append"),
        );
    }
    store
        .append_batch(&appends)
        .await
        .expect("seed scan budget plus lookahead");

    let first = handle_list_workflows(
        State(state.clone()),
        Some(context("tenant-a", "denied")),
        Query(WorkflowListQuery {
            limit: Some(1),
            cursor: None,
            status: None,
        }),
    )
    .await
    .expect("bounded denied page");
    let first = serde_json::to_value(first.0).expect("serialize first scan");
    assert_eq!(first["value"].as_array().map(Vec::len), Some(0));
    let cursor = first["next_cursor"].as_str().expect("scan continuation");

    let final_page = handle_list_workflows(
        State(state),
        Some(context("tenant-a", "denied")),
        Query(WorkflowListQuery {
            limit: Some(1),
            cursor: Some(cursor.to_string()),
            status: None,
        }),
    )
    .await
    .expect("remaining denied page");
    let final_page = serde_json::to_value(final_page.0).expect("serialize final scan");
    assert_eq!(final_page["value"].as_array().map(Vec::len), Some(0));
    assert!(final_page["next_cursor"].is_null());
}

#[tokio::test]
async fn unavailable_storage_uses_a_stable_sanitized_category() {
    let state =
        ServerState::from_registry(ActorSystem::new("collection-no-store"), SpecRegistry::new());
    permit_reader(&state, "tenant-a");
    let error = handle_list_workflows(
        State(state),
        Some(context("tenant-a", "reader")),
        Query(WorkflowListQuery {
            limit: None,
            cursor: None,
            status: None,
        }),
    )
    .await
    .expect_err("missing store");
    assert_eq!(error.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(error.category, "storage_unavailable");
}

#[tokio::test]
async fn cross_tenant_and_missing_ids_are_indistinguishable_after_restart() {
    let (state, store, _temp) = state().await;
    permit_reader(&state, "tenant-a");
    let foreign = workflow("tenant-b", "batch-b", &["secret"]);
    seed(&state, &foreign).await;

    for workflow_id in [&foreign.workflow_id, "missing-workflow"] {
        let error = handle_get_workflow(
            State(state.clone()),
            Some(context("tenant-a", "reader")),
            Path(workflow_id.to_string()),
        )
        .await
        .expect_err("tenant-scoped miss");
        assert_eq!(error.status, StatusCode::NOT_FOUND);
        assert_eq!(error.category, "workflow_not_found");
    }

    let local = workflow("tenant-a", "batch-a", &["secret"]);
    seed(&state, &local).await;
    let restarted = ServerState::from_registry(
        ActorSystem::new("collection-observe-restarted"),
        SpecRegistry::new(),
    );
    // The route reads the event journal directly; an actor registry restart is
    // represented by a fresh state sharing the same durable storage stack.
    let mut restarted = restarted;
    restarted.set_storage_stack(StorageStack::from_turso(store));
    permit_reader(&restarted, "tenant-a");
    let detail = handle_get_workflow(
        State(restarted),
        Some(context("tenant-a", "reader")),
        Path(local.workflow_id.clone()),
    )
    .await
    .expect("durable detail after restart");
    let detail = serde_json::to_value(detail.0).expect("serialize detail");
    assert_eq!(detail["workflow_id"], local.workflow_id);
}
