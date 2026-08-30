//! Generated-client lifecycle and cold-restart proof for scoped module data.

use std::collections::BTreeSet;
use std::time::Duration;

use temper_authz::SecurityContext;
use temper_runtime::ActorSystem;
use temper_runtime::persistence::schema_deployment::{
    SchemaExecutionPin, SchemaScope, SchemaScopeKind,
};
use temper_runtime::tenant::TenantId;
use temper_spec::bundle::{
    IoaSourceInput, ScopedBundleBudgets, ScopedSpecBundle, ScopedSpecBundleInput,
    WasmArtifactInput, scoped_module_data_closure_digest,
};
use temper_wasm_sdk::data::{DataOperationKind, EntityDataGrant, ModuleDataGrant};
use temper_wasm_sdk::schema_deployment::{
    ActivateSchemaBundleRequestV1, SchemaBundleBudgetsV1, SchemaIoaSourceV1, SchemaScopeV1,
    SchemaWasmArtifactV1, SubmitSchemaBundleRequestV1, VerifySchemaBundleRequestV1,
};

use super::GovernedSchemaDeploymentService;
use crate::registry::SpecRegistry;
use crate::request_context::AgentContext;
use crate::state::{DispatchExtOptions, ServerState};
use crate::storage::StorageStack;

const MODULE_NAME: &str = "scoped_client";
const CUSTOMER_ID: &str = "018f1f80-7b2d-7000-8000-000000000076";
const CSDL: &str = include_str!(
    "../../../temper-wasm/tests/fixtures/generated-scoped-data-integration-src/scoped.csdl.xml"
);
const CUSTOMER_IOA: &str = include_str!(
    "../../../temper-wasm/tests/fixtures/generated-scoped-data-integration-src/customer.ioa.toml"
);
const WORKER_IOA: &str = include_str!(
    "../../../temper-wasm/tests/fixtures/generated-scoped-data-integration-src/worker.ioa.toml"
);
const UNBOUND_GUEST: &[u8] =
    include_bytes!("../../../temper-wasm/tests/fixtures/generated_scoped_data_integration.wasm");

fn ioa_sources() -> Vec<IoaSourceInput> {
    vec![
        IoaSourceInput {
            entity_type: "Temper.Scoped.Customer".into(),
            source: CUSTOMER_IOA.into(),
        },
        IoaSourceInput {
            entity_type: "Temper.Scoped.Worker".into(),
            source: WORKER_IOA.into(),
        },
    ]
}

fn grant() -> ModuleDataGrant {
    ModuleDataGrant {
        operations: BTreeSet::from([
            DataOperationKind::EntityCreate,
            DataOperationKind::EntityGet,
        ]),
        entities: vec![EntityDataGrant {
            entity_type: "Temper.Scoped.Customer".into(),
            ..EntityDataGrant::default()
        }],
        ..ModuleDataGrant::default()
    }
}

fn transport_budgets(budgets: &ScopedBundleBudgets) -> SchemaBundleBudgetsV1 {
    SchemaBundleBudgetsV1 {
        verification_steps: budgets.verification_steps,
        migration_fuel_per_entity: budgets.migration_fuel_per_entity,
        migration_memory_pages: budgets.migration_memory_pages,
        migration_input_bytes: budgets.migration_input_bytes,
        migration_output_bytes: budgets.migration_output_bytes,
        migration_entities_per_batch: budgets.migration_entities_per_batch,
        migration_total_entities: budgets.migration_total_entities,
        migration_total_batches: budgets.migration_total_batches,
        migration_attempts: budgets.migration_attempts,
    }
}

async fn dispatch_worker(
    state: &ServerState,
    pin: &SchemaExecutionPin,
    worker_id: &str,
) -> crate::entity_actor::EntityResponse {
    let tenant = TenantId::default();
    state
        .get_or_create_scoped_entity(
            &tenant,
            "Worker",
            worker_id,
            serde_json::json!({}),
            pin.clone(),
        )
        .await
        .expect("pinned Worker should load");
    state
        .dispatch_tenant_action_ext(
            &tenant,
            "Worker",
            worker_id,
            "Run",
            serde_json::json!({}),
            DispatchExtOptions {
                agent_ctx: &AgentContext {
                    schema_pin: Some(pin.clone()),
                    ..AgentContext::system()
                },
                await_integration: true,
                await_reactions: true,
            },
        )
        .await
        .expect("generated scoped client should complete through the guest ABI")
}

async fn reopen_turso_after_actor_shutdown(
    database_url: &str,
) -> temper_store_turso::TursoEventStore {
    for attempt in 0..100 {
        match temper_store_turso::TursoEventStore::new(database_url, None).await {
            Ok(store) => return store,
            Err(error) if error.to_string().contains("database is locked") && attempt < 99 => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(error) => panic!("reopen persistent scoped-data store: {error}"),
        }
    }
    unreachable!("bounded Turso reopen loop returns or panics")
}

#[tokio::test(flavor = "multi_thread")]
async fn generated_client_survives_submission_activation_and_cold_restart() {
    let temp = tempfile::tempdir().expect("temporary Turso directory");
    let database_url = format!("file:{}", temp.path().join("scoped-data.db").display());
    let store = temper_store_turso::TursoEventStore::new(&database_url, None)
        .await
        .expect("create persistent scoped-data store");
    let mut state = ServerState::from_registry(
        ActorSystem::new("generated-scoped-data-e2e"),
        SpecRegistry::new(),
    );
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    state.data_dir = temp.path().join("data");

    let sources = ioa_sources();
    let budgets = ScopedBundleBudgets::default();
    let canonical = ScopedSpecBundle::compile(ScopedSpecBundleInput {
        scope_id: "generated-client-canonicalization".into(),
        predecessor_digest: None,
        csdl_xml: CSDL.into(),
        ioa_sources: sources.clone(),
        cedar_policies: Vec::new(),
        wasm_modules: Vec::new(),
        migration: None,
        budgets: budgets.clone(),
    })
    .expect("fixture closure should canonicalize");
    let closure = scoped_module_data_closure_digest(CSDL, sources.clone())
        .expect("fixture closure should canonicalize");
    let generated = temper_codegen::generate_module_sdk(
        &temper_spec::parse_csdl(canonical.canonical_csdl())
            .expect("canonical fixture CSDL should parse"),
        &sources,
        MODULE_NAME,
        &closure,
        &closure,
        "",
        grant(),
    )
    .expect("fixture client should generate");
    let packaged = temper_codegen::package_generated_module_sdk(UNBOUND_GUEST, generated)
        .expect("generated client should bind to its real guest artifact");
    let artifact_hash = packaged.manifest.artifact_digest.clone();
    let artifact_digest = format!("sha256:{artifact_hash}");
    state
        .upsert_wasm_module(
            TenantId::default().as_str(),
            MODULE_NAME,
            &packaged.wasm,
            &artifact_hash,
            "bundled",
        )
        .await
        .expect("scoped artifact should persist before submission");

    let scope_id = "generated-client-e2e";
    let binding_digest = packaged
        .manifest
        .binding_digest()
        .map(|digest| format!("sha256:{digest}"))
        .expect("generated manifest should be canonical");
    let compiled = ScopedSpecBundle::compile(ScopedSpecBundleInput {
        scope_id: scope_id.into(),
        predecessor_digest: None,
        csdl_xml: CSDL.into(),
        ioa_sources: sources.clone(),
        cedar_policies: Vec::new(),
        wasm_modules: vec![WasmArtifactInput {
            name: MODULE_NAME.into(),
            artifact_digest: artifact_digest.clone(),
            data_binding_digest: Some(binding_digest),
        }],
        migration: None,
        budgets: budgets.clone(),
    })
    .expect("scoped bundle should compile");
    let digest = compiled.digest().to_string();
    let service = GovernedSchemaDeploymentService::new(&state);
    let scope = SchemaScopeV1 {
        kind: "task".into(),
        id: scope_id.into(),
    };
    let submitted = service
        .submit(
            TenantId::default().as_str(),
            &SecurityContext::system(),
            SubmitSchemaBundleRequestV1 {
                request_id: "generated-client-submit".into(),
                idempotency_key: "generated-client-submit".into(),
                scope: scope.clone(),
                expected_predecessor: None,
                expected_digest: digest.clone(),
                canonicalization_version: temper_spec::bundle::SCOPED_SPEC_BUNDLE_CONTRACT_V1
                    .into(),
                csdl: CSDL.into(),
                ioa: sources
                    .iter()
                    .map(|source| SchemaIoaSourceV1 {
                        entity_type: source.entity_type.clone(),
                        source: source.source.clone(),
                    })
                    .collect(),
                cedar_policies: Vec::new(),
                wasm_modules: vec![SchemaWasmArtifactV1 {
                    name: MODULE_NAME.into(),
                    artifact_digest,
                    data_binding: Some(packaged.manifest.clone()),
                }],
                migration: None,
                budgets: transport_budgets(&budgets),
            },
        )
        .await
        .expect("bundle submission should persist the exact generated-client binding");
    assert_eq!(submitted.status, "submitted");
    let verified = service
        .verify(
            TenantId::default().as_str(),
            &SecurityContext::system(),
            VerifySchemaBundleRequestV1 {
                request_id: "generated-client-verify".into(),
                idempotency_key: "generated-client-verify".into(),
                scope: scope.clone(),
                bundle_digest: digest.clone(),
            },
        )
        .await
        .expect("host verification should regenerate and validate the client binding");
    let verification_receipt_id = verified
        .verification_receipt_id
        .clone()
        .expect("verified bundle should have a receipt");
    service
        .activate(
            TenantId::default().as_str(),
            &SecurityContext::system(),
            ActivateSchemaBundleRequestV1 {
                request_id: "generated-client-activate".into(),
                idempotency_key: "generated-client-activate".into(),
                scope: scope.clone(),
                bundle_digest: digest.clone(),
                expected_predecessor: None,
                expected_fence: verified.fence,
                verification_receipt_id,
                stream_descriptor_completion_receipt_id: None,
            },
        )
        .await
        .expect("verified generated-client bundle should activate");

    let pin = SchemaExecutionPin {
        scope: SchemaScope {
            kind: SchemaScopeKind::Task,
            id: scope_id.into(),
        },
        bundle_digest: digest.clone(),
    };
    let response = dispatch_worker(&state, &pin, "worker-before-restart").await;
    if response.state.status != "Done" {
        let invocations = store
            .load_recent_wasm_invocations(10)
            .await
            .expect("load guest failure diagnostics");
        panic!(
            "generated guest remained in status '{}': {invocations:?}",
            response.state.status
        );
    }
    let customer = crate::application_data::GovernedApplicationDataService::new(&state)
        .get_scoped(&TenantId::default(), "Customer", CUSTOMER_ID, pin.clone())
        .await
        .expect("generated client should have created the scoped Customer");
    assert_eq!(customer.state.fields["Name"], "generated-scoped-client");

    state.stop_and_remove_scoped_entity(
        &TenantId::default(),
        "Worker",
        "worker-before-restart",
        &pin,
    );
    state.stop_and_remove_scoped_entity(&TenantId::default(), "Customer", CUSTOMER_ID, &pin);
    tokio::task::yield_now().await;
    drop(state);
    drop(store);
    let reopened = reopen_turso_after_actor_shutdown(&database_url).await;
    let mut restarted = ServerState::from_registry(
        ActorSystem::new("generated-scoped-data-restart"),
        SpecRegistry::new(),
    );
    restarted.set_storage_stack(StorageStack::from_turso(reopened));
    restarted.data_dir = temp.path().join("data");
    assert!(
        restarted
            .registry
            .read()
            .expect("registry lock")
            .get_scoped_config_at_digest(&TenantId::default(), &pin.scope, &digest)
            .is_none(),
        "restart must begin with a cold scoped registry"
    );
    assert!(
        !restarted.wasm_engine.is_cached(&artifact_hash),
        "restart must begin with a cold artifact cache"
    );
    GovernedSchemaDeploymentService::new(&restarted)
        .recover_registry_pointer(TenantId::default().as_str(), &pin.scope)
        .await
        .expect("startup should recover the durable active scoped pointer");
    assert_eq!(
        restarted
            .registry
            .read()
            .expect("registry lock")
            .active_scope_digest(&TenantId::default(), &pin.scope),
        Some(digest.as_str()),
        "cold registry recovery must restore the exact active digest"
    );
    assert!(
        !restarted.wasm_engine.is_cached(&artifact_hash),
        "registry recovery must not hide eager artifact-cache state"
    );
    let response = dispatch_worker(&restarted, &pin, "worker-after-restart").await;
    assert_eq!(response.state.status, "Done");
    assert!(
        restarted.wasm_engine.is_cached(&artifact_hash),
        "guest dispatch must recover and compile the exact persisted artifact"
    );
    let recovered = crate::application_data::GovernedApplicationDataService::new(&restarted)
        .get_scoped(&TenantId::default(), "Customer", CUSTOMER_ID, pin)
        .await
        .expect("cold restart should recover the exact scoped Customer");
    assert_eq!(recovered.state.fields["Name"], "generated-scoped-client");
}
