mod common;

use common::reaction_fixture::*;
use temper_runtime::persistence::schema_deployment::{
    ActivateSchemaBundle, ClaimSchemaVerification, ClaimSchemaVerificationOutcome,
    CommitSchemaMigrationBatch, CreateSchemaMigration, CreateSchemaMigrationOutcome,
    RetireSchemaBundle, SchemaBundleRecord, SchemaDeploymentRecord, SchemaDeploymentStore,
    SchemaExecutionPin, SchemaMigrationBatchReceipt, SchemaMigrationBudgets,
    SchemaMigrationValidationReceipt, SchemaOperationIdentity, SchemaScope, SchemaScopeKind,
    SchemaVerificationReceipt, SubmitSchemaBundle,
};
use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};
use temper_server::registry::SpecRegistry;
use temper_spec::csdl::parse_csdl;

const SCOPED_ORDER_IOA: &str = r#"
[automaton]
name = "Order"
states = ["Draft", "Confirmed"]
initial = "Draft"

[[action]]
name = "ConfirmOrder"
kind = "input"
from = ["Draft"]
to = "Confirmed"

[[action.triggers]]
name = "authorize_payment"
kind = "entity"
target_entity = "Payment"
target_action = "AuthorizePayment"

[action.triggers.resolve_target]
type = "same_id"
"#;

const SCOPED_TIMEOUT_ORDER_IOA: &str = r#"
[automaton]
name = "Order"
states = ["Draft", "Expired"]
initial = "Draft"
allow_indefinite_states = ["Expired"]

[[action]]
name = "Expire"
kind = "input"
from = ["Draft"]
to = "Expired"

[[state_timeout]]
state = "Draft"
after_seconds = 1
on_timeout = "Expire"
"#;

fn scoped_state(
    tenant: &TenantId,
    scope: &SchemaScope,
    digest: &str,
    store: SimEventStore,
    order_ioa: &str,
) -> ServerState {
    let mut registry = SpecRegistry::new();
    registry
        .stage_scoped_bundle(
            tenant.clone(),
            scope.clone(),
            digest.to_string(),
            parse_csdl(CSDL_XML).expect("CSDL should parse"),
            CSDL_XML.to_string(),
            &[("Order", order_ioa), ("Payment", PAYMENT_IOA)],
        )
        .expect("scoped bundle should stage");
    registry
        .activate_scoped_bundle(tenant, scope, digest, None)
        .expect("scoped bundle should activate");
    let mut state = ServerState::from_registry(ActorSystem::new("scoped-reaction"), registry);
    state
        .authz
        .reload_tenant_policies(tenant.as_str(), "permit(principal, action, resource);")
        .expect("scoped reaction fixture policy should parse");
    state.set_storage_stack(StorageStack::from_sim(store, None));
    state.rebuild_reaction_dispatcher();
    state
}

async fn submit_verified_durable_pin(
    tenant: &TenantId,
    pin: &SchemaExecutionPin,
    store: &SimEventStore,
    order_ioa: &str,
    predecessor_digest: Option<String>,
    migration_target: bool,
) -> SchemaDeploymentRecord {
    let operation_suffix = pin.bundle_digest.chars().rev().take(8).collect::<String>();
    let submitted = store
        .submit_schema_bundle(SubmitSchemaBundle {
            bundle: SchemaBundleRecord {
                tenant: tenant.to_string(),
                scope: pin.scope.clone(),
                digest: pin.bundle_digest.clone(),
                predecessor_digest: predecessor_digest.clone(),
                canonical_csdl: CSDL_XML.into(),
                canonical_ioa: std::collections::BTreeMap::from([
                    ("Order".into(), order_ioa.into()),
                    ("Payment".into(), PAYMENT_IOA.into()),
                ]),
                cedar_policies: std::collections::BTreeMap::new(),
                wasm_module_digests: std::collections::BTreeMap::new(),
                migration_module_name: migration_target.then(|| "state-timeout-test".into()),
                migration_module_digest: migration_target
                    .then(|| format!("sha256:{}", "5".repeat(64))),
                migration_abi_version: migration_target
                    .then(|| "temper-schema-migration/v1".into()),
                canonical_budgets: "{}".into(),
            },
            idempotency_key: format!("submit-scoped-reaction-{operation_suffix}"),
            request_digest: format!("sha256:{}", "1".repeat(64)),
            request_id: format!("submit-scoped-reaction-{operation_suffix}"),
        })
        .await
        .expect("durable bundle should submit");
    let digest = match submitted {
        temper_runtime::persistence::schema_deployment::SubmitSchemaBundleOutcome::Created(
            record,
        )
        | temper_runtime::persistence::schema_deployment::SubmitSchemaBundleOutcome::Replayed(
            record,
        ) => record.bundle.digest,
    };
    let claimed = store
        .claim_schema_verification(ClaimSchemaVerification {
            tenant: tenant.to_string(),
            scope: pin.scope.clone(),
            bundle_digest: digest.clone(),
            logical_now: 1,
            lease_expires_at: 2,
            operation: SchemaOperationIdentity {
                idempotency_key: format!("verify-scoped-reaction-{operation_suffix}"),
                request_digest: format!("sha256:{}", "2".repeat(64)),
                request_id: format!("verify-scoped-reaction-{operation_suffix}"),
            },
        })
        .await
        .expect("durable bundle should claim verification");
    let fence = match claimed {
        ClaimSchemaVerificationOutcome::Claimed(record)
        | ClaimSchemaVerificationOutcome::Replayed(record) => record.fence,
    };
    store
        .finish_schema_verification(
            tenant.as_str(),
            &pin.scope,
            &digest,
            fence,
            SchemaVerificationReceipt {
                id: "scoped-reaction-verification".into(),
                verifier_version: "test/v1".into(),
                input_digest: format!("sha256:{}", "3".repeat(64)),
                passed: true,
            },
        )
        .await
        .expect("durable bundle should verify")
}

async fn activate_durable_pin(
    tenant: &TenantId,
    pin: &SchemaExecutionPin,
    store: &SimEventStore,
    order_ioa: &str,
    predecessor_digest: Option<String>,
) {
    let verified = submit_verified_durable_pin(
        tenant,
        pin,
        store,
        order_ioa,
        predecessor_digest.clone(),
        false,
    )
    .await;
    let operation_suffix = pin.bundle_digest.chars().rev().take(8).collect::<String>();
    store
        .activate_schema_bundle(ActivateSchemaBundle {
            tenant: tenant.to_string(),
            scope: pin.scope.clone(),
            bundle_digest: pin.bundle_digest.clone(),
            expected_predecessor: predecessor_digest,
            expected_fence: verified.fence,
            verification_receipt_id: "scoped-reaction-verification".into(),
            operation: SchemaOperationIdentity {
                idempotency_key: format!("activate-scoped-reaction-{operation_suffix}"),
                request_digest: format!("sha256:{}", "4".repeat(64)),
                request_id: format!("activate-scoped-reaction-{operation_suffix}"),
            },
        })
        .await
        .expect("durable bundle should activate");
}

async fn prepare_empty_migration(
    tenant: &TenantId,
    source_pin: &SchemaExecutionPin,
    target_pin: &SchemaExecutionPin,
    store: &SimEventStore,
    source_write_version: u64,
) -> u64 {
    submit_verified_durable_pin(
        tenant,
        target_pin,
        store,
        SCOPED_TIMEOUT_ORDER_IOA,
        Some(source_pin.bundle_digest.clone()),
        true,
    )
    .await;
    let source_pointer = store
        .active_schema_pointer(tenant.as_str(), &source_pin.scope)
        .await
        .expect("active pointer lookup")
        .expect("source pointer");
    let job_id = "state-timeout-cutover-race";
    let created = store
        .create_schema_migration(CreateSchemaMigration {
            job_id: job_id.into(),
            tenant: tenant.to_string(),
            scope: source_pin.scope.clone(),
            source_bundle_digest: source_pin.bundle_digest.clone(),
            target_bundle_digest: target_pin.bundle_digest.clone(),
            verification_receipt_id: "scoped-reaction-verification".into(),
            source_expected_fence: source_pointer.fence,
            module_name: "state-timeout-test".into(),
            module_digest: format!("sha256:{}", "5".repeat(64)),
            accepted_authority_json: r#"{"principal":"migration-test"}"#.into(),
            budgets: SchemaMigrationBudgets {
                fuel_per_entity: 10_000,
                memory_pages: 2,
                input_bytes: 4_096,
                output_bytes: 4_096,
                entities_per_batch: 1,
                total_entities: 1,
                total_batches: 1,
                attempts: 1,
            },
            idempotency_key: "state-timeout-cutover-race".into(),
            request_digest: format!("sha256:{}", "8".repeat(64)),
            request_id: "state-timeout-cutover-race".into(),
        })
        .await
        .expect("migration should be created");
    assert!(matches!(created, CreateSchemaMigrationOutcome::Created(_)));
    let claimed = store
        .claim_schema_migration(tenant.as_str(), job_id, 1, 2)
        .await
        .expect("migration should be claimed");
    let validating = store
        .commit_schema_migration_batch(
            tenant.as_str(),
            CommitSchemaMigrationBatch {
                job_id: job_id.into(),
                expected_fence: claimed.fence,
                expected_cursor: None,
                next_cursor: None,
                scan_complete: true,
                restart_scan: false,
                observed_source_write_version: source_write_version,
                rows: Vec::new(),
                receipt: SchemaMigrationBatchReceipt {
                    id: "state-timeout-empty-batch".into(),
                    source_cursor: None,
                    next_cursor: None,
                    input_digest: format!("sha256:{}", "9".repeat(64)),
                    output_digest: format!("sha256:{}", "a".repeat(64)),
                    row_count: 0,
                },
            },
        )
        .await
        .expect("empty migration scan should commit");
    let ready = store
        .validate_schema_migration(
            tenant.as_str(),
            job_id,
            validating.fence,
            SchemaMigrationValidationReceipt {
                id: "state-timeout-validation".into(),
                shadow_digest: format!("sha256:{}", "a".repeat(64)),
                caught_up_sequence: source_write_version,
                passed: true,
            },
        )
        .await
        .expect("migration should validate");
    ready.fence
}

#[path = "scoped_reaction_recovery/reaction.rs"]
mod reaction;
#[path = "scoped_reaction_recovery/timeouts.rs"]
mod timeouts;
