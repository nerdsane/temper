use std::collections::BTreeSet;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use temper_authz::SecurityContext;
use temper_runtime::persistence::schema_deployment::{
    ClaimSchemaVerification, ClaimSchemaVerificationOutcome, SchemaExecutionPin,
    SchemaOperationIdentity, SchemaScope, SchemaScopeKind, SchemaVerificationReceipt,
};
use temper_runtime::tenant::TenantId;
use temper_spec::{
    IoaSourceInput, MigrationArtifactInput, ScopedBundleBudgets, ScopedSpecBundle,
    ScopedSpecBundleInput,
};
use temper_wasm_sdk::data::DataOperationKind;
use temper_wasm_sdk::schema_deployment::{
    ActivateSchemaBundleRequestV1, GetSchemaBundleRequestV1, RetireSchemaBundleRequestV1,
    RetrySchemaMigrationRequestV1, SCHEMA_DEPLOYMENT_ABI_V1, SchemaBundleBudgetsV1,
    SchemaDeploymentOperationV1, SchemaDeploymentRequestV1, SchemaDeploymentResponseV1,
    SchemaIoaSourceV1, SchemaMigrationArtifactV1, SchemaMigrationBudgetsV1, SchemaScopeV1,
    StartSchemaMigrationRequestV1, SubmitSchemaBundleRequestV1,
};
use tower::ServiceExt;

mod migration;

use super::ApplicationDataInvocation;
use super::tests::invocation;

const IOA: &str = r#"[automaton]
name = "Task"
states = ["Open"]
initial = "Open"
"#;

const CSDL: &str = r#"<?xml version="1.0"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
<edmx:DataServices><Schema Namespace="Example" xmlns="http://docs.oasis-open.org/odata/ns/edm">
<EntityType Name="Task"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String" Nullable="false"/><Property Name="Title" Type="Edm.String"/></EntityType>
<EntityContainer Name="Default"><EntitySet Name="Tasks" EntityType="Example.Task"/></EntityContainer>
</Schema></edmx:DataServices></edmx:Edmx>"#;

fn request() -> SubmitSchemaBundleRequestV1 {
    let budgets = ScopedBundleBudgets::default();
    let compiled = ScopedSpecBundle::compile(ScopedSpecBundleInput {
        scope_id: "task-42".into(),
        predecessor_digest: None,
        csdl_xml: CSDL.into(),
        ioa_sources: vec![IoaSourceInput {
            entity_type: "Example.Task".into(),
            source: IOA.into(),
        }],
        cedar_policies: vec![],
        wasm_modules: vec![],
        migration: None,
        budgets: budgets.clone(),
    })
    .unwrap();
    SubmitSchemaBundleRequestV1 {
        request_id: "request-42".into(),
        idempotency_key: "submit-42".into(),
        scope: SchemaScopeV1 {
            kind: "task".into(),
            id: "task-42".into(),
        },
        expected_predecessor: None,
        expected_digest: compiled.digest().into(),
        canonicalization_version: temper_spec::bundle::SCOPED_SPEC_BUNDLE_CONTRACT_V1.into(),
        csdl: CSDL.into(),
        ioa: vec![SchemaIoaSourceV1 {
            entity_type: "Example.Task".into(),
            source: IOA.into(),
        }],
        cedar_policies: vec![],
        wasm_modules: vec![],
        migration: None,
        budgets: SchemaBundleBudgetsV1 {
            verification_steps: budgets.verification_steps,
            migration_fuel_per_entity: budgets.migration_fuel_per_entity,
            migration_memory_pages: budgets.migration_memory_pages,
            migration_input_bytes: budgets.migration_input_bytes,
            migration_output_bytes: budgets.migration_output_bytes,
            migration_entities_per_batch: budgets.migration_entities_per_batch,
            migration_total_entities: budgets.migration_total_entities,
            migration_total_batches: budgets.migration_total_batches,
            migration_attempts: budgets.migration_attempts,
        },
    }
}

fn schema_invocation() -> std::sync::Arc<ApplicationDataInvocation> {
    let original = invocation(
        BTreeSet::from([
            DataOperationKind::SchemaBundleSubmit,
            DataOperationKind::SchemaBundleGet,
        ]),
        SecurityContext::system(),
    );
    let mut state = original.state.clone();
    state.set_storage_stack(crate::storage::StorageStack::from_sim(
        temper_store_sim::SimEventStore::no_faults(114),
        None,
    ));
    ApplicationDataInvocation::new(state, original.authority.clone())
}

fn schema_migration_invocation() -> std::sync::Arc<ApplicationDataInvocation> {
    let original = invocation(
        BTreeSet::from([
            DataOperationKind::SchemaBundleSubmit,
            DataOperationKind::SchemaBundleActivate,
            DataOperationKind::SchemaMigrationStart,
            DataOperationKind::SchemaMigrationGet,
            DataOperationKind::SchemaMigrationRetry,
        ]),
        SecurityContext::system(),
    );
    let mut state = original.state.clone();
    state.set_storage_stack(crate::storage::StorageStack::from_sim(
        temper_store_sim::SimEventStore::no_faults(115),
        None,
    ));
    ApplicationDataInvocation::new(state, original.authority.clone())
}

#[tokio::test]
async fn wasm_and_http_submit_share_receipt_and_host_bound_tenant() {
    let invocation = schema_invocation();
    let submit = request();
    let encoded = serde_json::to_vec(&SchemaDeploymentRequestV1 {
        abi: SCHEMA_DEPLOYMENT_ABI_V1.into(),
        operation: SchemaDeploymentOperationV1::Submit(submit.clone()),
    })
    .unwrap();
    let wasm: SchemaDeploymentResponseV1 =
        serde_json::from_slice(&invocation.call_encoded(&encoded).await.unwrap()).unwrap();
    let SchemaDeploymentResponseV1::Ok {
        receipt: wasm_receipt,
    } = wasm
    else {
        panic!("typed WASM submit should succeed")
    };

    let response =
        super::tests::authenticated_router(invocation.state.clone(), SecurityContext::system())
            .oneshot(
                Request::post("/api/v1/schema-deployments")
                    .header("content-type", "application/json")
                    .header("x-tenant-id", "default")
                    .body(Body::from(serde_json::to_vec(&submit).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 1_048_576).await.unwrap();
    let http: SchemaDeploymentResponseV1 = serde_json::from_slice(&body).unwrap();
    let SchemaDeploymentResponseV1::Ok {
        receipt: http_receipt,
    } = http
    else {
        panic!("HTTP idempotent replay should succeed")
    };
    assert_eq!(wasm_receipt, http_receipt);
    assert_eq!(wasm_receipt.request_id, "request-42");

    let encoded_get = serde_json::to_vec(&SchemaDeploymentRequestV1 {
        abi: SCHEMA_DEPLOYMENT_ABI_V1.into(),
        operation: SchemaDeploymentOperationV1::GetBundle(GetSchemaBundleRequestV1 {
            request_id: "get-42".into(),
            scope: submit.scope,
            bundle_digest: wasm_receipt.bundle_digest.clone(),
        }),
    })
    .unwrap();
    let get: SchemaDeploymentResponseV1 =
        serde_json::from_slice(&invocation.call_encoded(&encoded_get).await.unwrap()).unwrap();
    let SchemaDeploymentResponseV1::Ok { receipt } = get else {
        panic!("typed WASM get should succeed")
    };
    assert_eq!(receipt.request_id, "get-42");
    assert_eq!(receipt.bundle_digest, wasm_receipt.bundle_digest);
    assert_eq!(receipt.status, "submitted");
}

#[tokio::test]
async fn wasm_schema_grant_denies_before_cedar_system_authority() {
    let invocation = invocation(BTreeSet::new(), SecurityContext::system());
    let encoded = serde_json::to_vec(&SchemaDeploymentRequestV1 {
        abi: SCHEMA_DEPLOYMENT_ABI_V1.into(),
        operation: SchemaDeploymentOperationV1::Submit(request()),
    })
    .unwrap();
    let response: SchemaDeploymentResponseV1 =
        serde_json::from_slice(&invocation.call_encoded(&encoded).await.unwrap()).unwrap();
    let SchemaDeploymentResponseV1::Error { error } = response else {
        panic!("missing module grant must deny")
    };
    assert_eq!(error.code, "authorization_denied");
}

#[tokio::test]
async fn activation_response_replay_after_retirement_does_not_resurrect_registry_pointer() {
    let invocation = schema_invocation();
    let state = &invocation.state;
    let service = crate::schema_deployment::GovernedSchemaDeploymentService::new(state);
    let security = SecurityContext::system();
    let submitted = service
        .submit("default", &security, request())
        .await
        .unwrap();
    let scope = SchemaScope {
        kind: SchemaScopeKind::Task,
        id: "task-42".into(),
    };
    let store = state
        .storage_stack
        .as_ref()
        .unwrap()
        .schema_deployments
        .as_ref()
        .unwrap();
    let claim = store
        .claim_schema_verification(ClaimSchemaVerification {
            tenant: "default".into(),
            scope: scope.clone(),
            bundle_digest: submitted.bundle_digest.clone(),
            logical_now: 1,
            lease_expires_at: 10,
            operation: SchemaOperationIdentity {
                idempotency_key: "verify-response-loss".into(),
                request_digest: format!("sha256:{}", "1".repeat(64)),
                request_id: "verify-response-loss".into(),
            },
        })
        .await
        .unwrap();
    let claim = match claim {
        ClaimSchemaVerificationOutcome::Claimed(record) => record,
        ClaimSchemaVerificationOutcome::Replayed(_) => panic!("first verification must claim"),
    };
    let verified = store
        .finish_schema_verification(
            "default",
            &scope,
            &submitted.bundle_digest,
            claim.fence,
            SchemaVerificationReceipt {
                id: "verification-receipt".into(),
                verifier_version: "test/v1".into(),
                input_digest: format!("sha256:{}", "2".repeat(64)),
                passed: true,
            },
        )
        .await
        .unwrap();
    let activation = ActivateSchemaBundleRequestV1 {
        request_id: "activate-response-loss".into(),
        idempotency_key: "activate-response-loss".into(),
        scope: SchemaScopeV1 {
            kind: "task".into(),
            id: "task-42".into(),
        },
        bundle_digest: submitted.bundle_digest.clone(),
        expected_predecessor: None,
        expected_fence: verified.fence,
        verification_receipt_id: verified.verification_receipt_id.clone().unwrap(),
    };
    let active = service
        .activate("default", &security, activation.clone())
        .await
        .unwrap();
    service
        .retire(
            "default",
            &security,
            RetireSchemaBundleRequestV1 {
                request_id: "retire-after-response-loss".into(),
                idempotency_key: "retire-after-response-loss".into(),
                scope: activation.scope.clone(),
                bundle_digest: activation.bundle_digest.clone(),
                expected_fence: active.fence,
            },
        )
        .await
        .unwrap();

    service
        .activate("default", &security, activation)
        .await
        .unwrap();
    assert_eq!(
        state
            .registry
            .read()
            .unwrap()
            .active_scope_digest(&TenantId::default(), &scope),
        None
    );
}
