use std::sync::atomic::{AtomicU64, Ordering};

use super::*;
use temper_authz::{ActionScope, DurationScope, PrincipalScope, ResourceScope};
use temper_runtime::ActorSystem;
use temper_spec::csdl::CsdlDocument;
use temper_store_turso::TursoEventStore;

static DB_ID: AtomicU64 = AtomicU64::new(0);

fn approval_scope() -> temper_authz::PolicyScopeMatrix {
    approval_scope_for("session-1")
}

fn approval_scope_for(session_id: &str) -> temper_authz::PolicyScopeMatrix {
    temper_authz::PolicyScopeMatrix {
        principal: PrincipalScope::ThisAgent,
        action: ActionScope::ThisAction,
        resource: ResourceScope::ThisResource,
        duration: DurationScope::Session,
        agent_type_value: None,
        role_value: None,
        session_id: Some(session_id.to_string()),
    }
}

async fn state_with_turso(policy_store_enabled: bool) -> ServerState {
    let id = DB_ID.fetch_add(1, Ordering::SeqCst);
    let url = format!(
        "file:/tmp/temper-decision-approval-{}-{id}.db",
        std::process::id()
    );
    let _ = std::fs::remove_file(url.trim_start_matches("file:"));
    let store = TursoEventStore::new(&url, None)
        .await
        .expect("create approval test store");
    let mut stack = crate::StorageStack::from_turso(store);
    if !policy_store_enabled {
        stack.policies = None;
    }
    let mut state = ServerState::new(
        ActorSystem::new("decision-approval-test"),
        CsdlDocument {
            version: "4.0".to_string(),
            schemas: Vec::new(),
        },
        String::new(),
    );
    state.set_storage_stack(stack);
    state
}

async fn states_with_shared_turso() -> (ServerState, ServerState) {
    let id = DB_ID.fetch_add(1, Ordering::SeqCst);
    let url = format!(
        "file:/tmp/temper-decision-resolution-shared-{}-{id}.db",
        std::process::id()
    );
    let _ = std::fs::remove_file(url.trim_start_matches("file:"));
    let store = TursoEventStore::new(&url, None)
        .await
        .expect("create shared approval store");
    let make_state = |name: &str| {
        let mut state = ServerState::new(
            ActorSystem::new(name),
            CsdlDocument {
                version: "4.0".to_string(),
                schemas: Vec::new(),
            },
            String::new(),
        );
        state.set_storage_stack(crate::StorageStack::from_turso(store.clone()));
        state
    };
    (
        make_state("decision-resolution-shared-a"),
        make_state("decision-resolution-shared-b"),
    )
}

async fn seed_pending(state: &ServerState, id: &str) {
    let mut decision = PendingDecision::from_denial(
        "tenant-a",
        "customer-1",
        "read",
        "Order",
        "order-1",
        serde_json::json!({"id": "order-1"}),
        "denied",
        None,
    );
    decision.id = id.to_string();
    decision.principal_kind = Some("Customer".to_string());
    decision.session_id = Some("session-1".to_string());
    state
        .persist_pending_decision(&decision)
        .await
        .expect("seed pending decision");
}

async fn load_decision(state: &ServerState, id: &str) -> PendingDecision {
    let store = state
        .metadata_store_for_tenant("tenant-a")
        .await
        .expect("metadata store");
    let data = store
        .get_pending_decision(id)
        .await
        .expect("load pending decision")
        .expect("pending decision row");
    serde_json::from_str(&data).expect("deserialize pending decision")
}

fn request_body() -> ApproveBody {
    request_body_for("session-1")
}

fn request_body_for(session_id: &str) -> ApproveBody {
    ApproveBody {
        scope: approval_scope_for(session_id),
        decided_by: Some("reviewer".to_string()),
    }
}

#[tokio::test]
async fn independent_instances_choose_exactly_one_approve_or_deny_owner() {
    let (first, second) = states_with_shared_turso().await;
    seed_pending(&first, "pd-approve-deny-race").await;

    let (approved, denied) = tokio::join!(
        handle_approve_decision(
            State(first.clone()),
            Path(("tenant-a".to_string(), "pd-approve-deny-race".to_string(),)),
            PolicyAuthed,
            axum::Json(request_body()),
        ),
        super::super::handle_deny_decision(
            State(second.clone()),
            Path(("tenant-a".to_string(), "pd-approve-deny-race".to_string(),)),
            PolicyAuthed,
            Some(axum::Json(serde_json::json!({"decided_by": "reviewer"}))),
        ),
    );
    let denied = denied.into_response();
    assert_eq!(
        usize::from(approved.status() == StatusCode::OK)
            + usize::from(denied.status() == StatusCode::OK),
        1
    );

    let terminal = load_decision(&first, "pd-approve-deny-race").await;
    assert!(matches!(
        terminal.status,
        DecisionStatus::Approved | DecisionStatus::Denied
    ));
    let policies = first
        .policy_store()
        .expect("policy store")
        .load_policy_snapshot("tenant-a")
        .await
        .expect("policy snapshot");
    assert_eq!(
        policies.rows.len(),
        usize::from(terminal.status == DecisionStatus::Approved),
        "deny must not leave an approval policy and approve must publish exactly one"
    );
}

#[tokio::test]
async fn policy_store_failure_leaves_rest_decision_pending() {
    let state = state_with_turso(false).await;
    seed_pending(&state, "pd-no-policy-store").await;

    let response = handle_approve_decision(
        State(state.clone()),
        Path(("tenant-a".to_string(), "pd-no-policy-store".to_string())),
        PolicyAuthed,
        axum::Json(request_body()),
    )
    .await;

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        load_decision(&state, "pd-no-policy-store").await.status,
        DecisionStatus::Pending
    );
    assert!(state.authz.get_tenant_policy_text("tenant-a").is_none());
}

#[tokio::test]
async fn missing_principal_kind_cannot_default_into_another_namespace() {
    let state = state_with_turso(true).await;
    let mut decision = PendingDecision::from_denial(
        "tenant-a",
        "shared-id",
        "read",
        "Order",
        "order-1",
        serde_json::json!({"id": "order-1"}),
        "denied",
        None,
    );
    decision.id = "pd-missing-kind".to_string();
    state
        .persist_pending_decision(&decision)
        .await
        .expect("seed legacy decision");

    let response = handle_approve_decision(
        State(state.clone()),
        Path(("tenant-a".to_string(), decision.id.clone())),
        PolicyAuthed,
        axum::Json(request_body()),
    )
    .await;

    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(state.authz.get_tenant_policy_text("tenant-a").is_none());
    assert!(
        state
            .policy_store()
            .expect("policy store")
            .load_policies_for_tenant("tenant-a")
            .await
            .expect("load policies")
            .is_empty()
    );
}

#[tokio::test]
async fn exact_approval_retry_keeps_one_policy_and_one_status_transition() {
    let state = state_with_turso(true).await;
    seed_pending(&state, "pd-idempotent").await;

    for _ in 0..2 {
        let response = handle_approve_decision(
            State(state.clone()),
            Path(("tenant-a".to_string(), "pd-idempotent".to_string())),
            PolicyAuthed,
            axum::Json(request_body()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    let decision = load_decision(&state, "pd-idempotent").await;
    assert_eq!(decision.status, DecisionStatus::Approved);
    assert_eq!(decision.principal_kind.as_deref(), Some("Customer"));
    assert_eq!(
        decision.generated_policy,
        Some(
            decision
                .generate_policy_from_matrix(&approval_scope())
                .expect("stored approval should regenerate exactly")
        )
    );
    let rows = state
        .policy_store()
        .expect("policy store")
        .load_policies_for_tenant("tenant-a")
        .await
        .expect("load policies");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].policy_id, "decision:pd-idempotent");
}

#[tokio::test]
async fn resumed_owner_survives_unrelated_policy_publication() {
    let state = state_with_turso(true).await;
    let id = "pd-resume-after-policy-advance";
    seed_pending(&state, id).await;
    let mut decision = load_decision(&state, id).await;
    let body = request_body();
    let generated_policy = decision
        .generate_policy_from_matrix(&body.scope)
        .expect("generate decision policy");
    let owner = resolution_owner(
        &decision,
        DecisionResolutionKind::Approve,
        &format!("reviewer\0{generated_policy}"),
    );
    let installed = crate::authz::install_decision_policy(
        &state,
        "tenant-a",
        &format!("decision:{id}"),
        &generated_policy,
        "reviewer",
    )
    .await
    .expect("install decision policy");
    let DecisionPolicyInstall::Created {
        publication_version,
    } = installed
    else {
        panic!("fixture policy should be newly created");
    };
    crate::authz::upsert_policy_entries(
        &state,
        "tenant-a",
        &[crate::authz::PolicyEntryUpsert {
            policy_id: "unrelated",
            cedar_text: "forbid(principal, action, resource);",
            created_by: "other-reviewer",
        }],
    )
    .await
    .expect("advance policy publication");

    decision.resolution_owner = Some(owner);
    decision.resolution_kind = Some(DecisionResolutionKind::Approve);
    decision.resolution_phase = Some(DecisionResolutionPhase::PolicyPublished);
    decision.resolution_policy_version = Some(publication_version);
    let store = state
        .metadata_store_for_tenant("tenant-a")
        .await
        .expect("metadata store");
    assert!(
        store
            .claim_decision_resolution(
                "tenant-a",
                id,
                &serde_json::to_string(&decision).expect("serialize claimed decision"),
            )
            .await
            .expect("claim decision")
    );

    let response = handle_approve_decision(
        State(state.clone()),
        Path(("tenant-a".to_string(), id.to_string())),
        PolicyAuthed,
        axum::Json(body),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        load_decision(&state, id).await.status,
        DecisionStatus::Approved
    );
    assert_eq!(
        state
            .policy_store()
            .expect("policy store")
            .load_policy_snapshot("tenant-a")
            .await
            .expect("policy snapshot")
            .rows
            .len(),
        2
    );
}

#[tokio::test]
async fn concurrent_scopes_cannot_overwrite_one_decision_policy() {
    let state = state_with_turso(true).await;
    seed_pending(&state, "pd-concurrent").await;
    let first_state = state.clone();
    let second_state = state.clone();
    let (first, second) = tokio::join!(
        handle_approve_decision(
            State(first_state),
            Path(("tenant-a".to_string(), "pd-concurrent".to_string())),
            PolicyAuthed,
            axum::Json(request_body_for("session-a")),
        ),
        handle_approve_decision(
            State(second_state),
            Path(("tenant-a".to_string(), "pd-concurrent".to_string())),
            PolicyAuthed,
            axum::Json(request_body_for("session-b")),
        )
    );
    let statuses = [first.status(), second.status()];
    assert!(statuses.contains(&StatusCode::OK));
    assert!(statuses.contains(&StatusCode::CONFLICT));

    let decision = load_decision(&state, "pd-concurrent").await;
    let rows = state
        .policy_store()
        .expect("policy store")
        .load_policies_for_tenant("tenant-a")
        .await
        .expect("load policies");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].cedar_text, decision.generated_policy.unwrap());
}

#[tokio::test]
async fn governance_dispatch_failure_retains_approval_owner_and_blocks_competing_deny() {
    let state = state_with_turso(true).await;
    let id = "pd-governance-dispatch-failure";
    seed_pending(&state, id).await;
    let mut pending = load_decision(&state, id).await;
    pending.governance_decision_id = Some("missing-governance-actor".to_string());
    state
        .persist_pending_decision(&pending)
        .await
        .expect("link missing governance actor");

    let approval = handle_approve_decision(
        State(state.clone()),
        Path(("tenant-a".to_string(), id.to_string())),
        PolicyAuthed,
        axum::Json(request_body()),
    )
    .await;
    assert_eq!(approval.status(), StatusCode::BAD_GATEWAY);

    let retained = load_decision(&state, id).await;
    assert_eq!(retained.status, DecisionStatus::Pending);
    assert_eq!(
        retained.resolution_kind,
        Some(DecisionResolutionKind::Approve)
    );
    assert_eq!(
        retained.resolution_phase,
        Some(DecisionResolutionPhase::PolicyPublished)
    );
    assert!(retained.resolution_owner.is_some());
    assert!(retained.resolution_policy_version.is_some());
    let rows = state
        .policy_store()
        .expect("policy store")
        .load_policy_snapshot("tenant-a")
        .await
        .expect("load retained approval policy")
        .rows;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].policy_id, format!("decision:{id}"));

    let denial = super::super::handle_deny_decision(
        State(state.clone()),
        Path(("tenant-a".to_string(), id.to_string())),
        PolicyAuthed,
        Some(axum::Json(serde_json::json!({"decided_by": "reviewer"}))),
    )
    .await
    .into_response();
    assert_eq!(denial.status(), StatusCode::CONFLICT);
    let after_denial = load_decision(&state, id).await;
    assert_eq!(after_denial.resolution_owner, retained.resolution_owner);
    assert_eq!(
        after_denial.resolution_kind,
        Some(DecisionResolutionKind::Approve)
    );
}
