use std::collections::BTreeSet;

use temper_authz::SecurityContext;
use temper_runtime::persistence::schema_deployment::{
    ClaimSchemaVerification, ClaimSchemaVerificationOutcome, SchemaExecutionPin,
    SchemaOperationIdentity, SchemaScope, SchemaScopeKind, SchemaVerificationReceipt,
};
use temper_spec::{IoaSourceInput, ScopedBundleBudgets, ScopedSpecBundle, ScopedSpecBundleInput};
use temper_wasm_sdk::data::{
    DataOperationKind, DataOperationV1, DataOutcomeV1, DataResultV1, EntityDataGrant,
    FileOperationKind, ModuleDataErrorKind, ModuleDataGrant, PageV1, Retryability,
};
use temper_wasm_sdk::schema_deployment::{
    ActivateSchemaBundleRequestV1, SchemaBundleBudgetsV1, SchemaIoaSourceV1, SchemaScopeV1,
    SubmitSchemaBundleRequestV1,
};

use super::tests::{CSDL, IOA, call, invocation, response_error};
use super::{ApplicationDataInvocation, ModuleDataTarget, ModuleInvocationAuthority};

const FILE_CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx"><edmx:DataServices><Schema Namespace="Temper.ScopedFile" xmlns="http://docs.oasis-open.org/odata/ns/edm"><EntityType Name="File" HasStream="true"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String" Nullable="false"/><Property Name="Status" Type="Edm.String" Nullable="false" DefaultValue="Created"/><Property Name="workspace_id" Type="Edm.String" Nullable="true"/><Annotation Term="Temper.Vocab.Stream.Mutability" String="Mutable"/></EntityType><Action Name="StreamUpdated" IsBound="true"><Parameter Name="bindingParameter" Type="Temper.ScopedFile.File" Nullable="false"/><Parameter Name="content_hash" Type="Edm.String" Nullable="false"/><Parameter Name="size_bytes" Type="Edm.String" Nullable="false"/><Parameter Name="mime_type" Type="Edm.String" Nullable="false"/><Parameter Name="version_number" Type="Edm.String" Nullable="false"/><Parameter Name="previous_version_id" Type="Edm.String" Nullable="false"/><Parameter Name="created_by" Type="Edm.String" Nullable="false"/></Action><EntityContainer Name="Container"><EntitySet Name="Files" EntityType="Temper.ScopedFile.File"/></EntityContainer></Schema></edmx:DataServices></edmx:Edmx>"#;

const FILE_IOA: &str = r#"[automaton]
name = "File"
states = ["Created", "Ready"]
initial = "Created"

[[action]]
name = "StreamUpdated"
kind = "input"
from = ["Created", "Ready"]
to = "Ready"
params = ["content_hash", "size_bytes", "mime_type", "version_number", "previous_version_id", "created_by"]
"#;

fn pin(scope_id: &str, digest: &str) -> SchemaExecutionPin {
    SchemaExecutionPin {
        scope: SchemaScope {
            kind: SchemaScopeKind::Task,
            id: scope_id.into(),
        },
        bundle_digest: digest.into(),
    }
}

async fn install_scope(state: &crate::state::ServerState, scope_id: &str) -> SchemaExecutionPin {
    let budgets = ScopedBundleBudgets::default();
    let compiled = ScopedSpecBundle::compile(ScopedSpecBundleInput {
        scope_id: scope_id.into(),
        predecessor_digest: None,
        csdl_xml: CSDL.into(),
        ioa_sources: vec![IoaSourceInput {
            entity_type: "Temper.Example.Customer".into(),
            source: IOA.into(),
        }],
        cedar_policies: Vec::new(),
        wasm_modules: Vec::new(),
        migration: None,
        budgets: budgets.clone(),
    })
    .expect("scope compiles");
    let digest = compiled.digest().to_string();
    let scope = SchemaScope {
        kind: SchemaScopeKind::Task,
        id: scope_id.into(),
    };
    let service = crate::schema_deployment::GovernedSchemaDeploymentService::new(state);
    let security = SecurityContext::system();
    let submitted = service
        .submit(
            "default",
            &security,
            SubmitSchemaBundleRequestV1 {
                request_id: format!("submit-{scope_id}"),
                idempotency_key: format!("submit-{scope_id}"),
                scope: SchemaScopeV1 {
                    kind: "task".into(),
                    id: scope_id.into(),
                },
                expected_predecessor: None,
                expected_digest: digest.clone(),
                canonicalization_version: temper_spec::bundle::SCOPED_SPEC_BUNDLE_CONTRACT_V1
                    .into(),
                csdl: CSDL.into(),
                ioa: vec![SchemaIoaSourceV1 {
                    entity_type: "Temper.Example.Customer".into(),
                    source: IOA.into(),
                }],
                cedar_policies: Vec::new(),
                wasm_modules: Vec::new(),
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
            },
        )
        .await
        .expect("scope submits");
    let store = state
        .storage_stack
        .as_ref()
        .and_then(|stack| stack.schema_deployments.as_ref())
        .expect("schema store");
    let claimed = store
        .claim_schema_verification(ClaimSchemaVerification {
            tenant: "default".into(),
            scope: scope.clone(),
            bundle_digest: submitted.bundle_digest.clone(),
            logical_now: 1,
            lease_expires_at: 10,
            operation: SchemaOperationIdentity {
                idempotency_key: format!("verify-{scope_id}"),
                request_digest: format!("sha256:{}", "d".repeat(64)),
                request_id: format!("verify-{scope_id}"),
            },
        })
        .await
        .expect("verification claims");
    let record = match claimed {
        ClaimSchemaVerificationOutcome::Claimed(record) => record,
        ClaimSchemaVerificationOutcome::Replayed(record) => record,
    };
    let verified = store
        .finish_schema_verification(
            "default",
            &scope,
            &submitted.bundle_digest,
            record.fence,
            SchemaVerificationReceipt {
                id: format!("receipt-{scope_id}"),
                verifier_version: "scoped-data-test/v1".into(),
                input_digest: format!("sha256:{}", "e".repeat(64)),
                passed: true,
            },
        )
        .await
        .expect("verification finishes");
    service
        .activate(
            "default",
            &security,
            ActivateSchemaBundleRequestV1 {
                request_id: format!("activate-{scope_id}"),
                idempotency_key: format!("activate-{scope_id}"),
                scope: SchemaScopeV1 {
                    kind: "task".into(),
                    id: scope_id.into(),
                },
                bundle_digest: submitted.bundle_digest.clone(),
                expected_predecessor: None,
                expected_fence: verified.fence,
                verification_receipt_id: verified
                    .verification_receipt_id
                    .expect("receipt id committed"),
                stream_descriptor_completion_receipt_id: None,
            },
        )
        .await
        .expect("scope activates");
    pin(scope_id, &submitted.bundle_digest)
}

async fn install_file_scope(state: &crate::state::ServerState) -> SchemaExecutionPin {
    let scope_id = "file-scope";
    let budgets = ScopedBundleBudgets::default();
    let compiled = ScopedSpecBundle::compile(ScopedSpecBundleInput {
        scope_id: scope_id.into(),
        predecessor_digest: None,
        csdl_xml: FILE_CSDL.into(),
        ioa_sources: vec![IoaSourceInput {
            entity_type: "Temper.ScopedFile.File".into(),
            source: FILE_IOA.into(),
        }],
        cedar_policies: Vec::new(),
        wasm_modules: Vec::new(),
        migration: None,
        budgets: budgets.clone(),
    })
    .expect("File scope compiles");
    let digest = compiled.digest().to_string();
    let scope = SchemaScope {
        kind: SchemaScopeKind::Task,
        id: scope_id.into(),
    };
    let service = crate::schema_deployment::GovernedSchemaDeploymentService::new(state);
    let security = SecurityContext::system();
    let submitted = service
        .submit(
            "default",
            &security,
            SubmitSchemaBundleRequestV1 {
                request_id: "submit-file-scope".into(),
                idempotency_key: "submit-file-scope".into(),
                scope: SchemaScopeV1 {
                    kind: "task".into(),
                    id: scope_id.into(),
                },
                expected_predecessor: None,
                expected_digest: digest,
                canonicalization_version: temper_spec::bundle::SCOPED_SPEC_BUNDLE_CONTRACT_V1
                    .into(),
                csdl: FILE_CSDL.into(),
                ioa: vec![SchemaIoaSourceV1 {
                    entity_type: "Temper.ScopedFile.File".into(),
                    source: FILE_IOA.into(),
                }],
                cedar_policies: Vec::new(),
                wasm_modules: Vec::new(),
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
            },
        )
        .await
        .expect("File scope submits");
    let store = state
        .storage_stack
        .as_ref()
        .and_then(|stack| stack.schema_deployments.as_ref())
        .expect("schema store");
    let claimed = store
        .claim_schema_verification(ClaimSchemaVerification {
            tenant: "default".into(),
            scope: scope.clone(),
            bundle_digest: submitted.bundle_digest.clone(),
            logical_now: 1,
            lease_expires_at: 10,
            operation: SchemaOperationIdentity {
                idempotency_key: "verify-file-scope".into(),
                request_digest: format!("sha256:{}", "f".repeat(64)),
                request_id: "verify-file-scope".into(),
            },
        })
        .await
        .expect("File verification claims");
    let record = match claimed {
        ClaimSchemaVerificationOutcome::Claimed(record) => record,
        ClaimSchemaVerificationOutcome::Replayed(record) => record,
    };
    let verified = store
        .finish_schema_verification(
            "default",
            &scope,
            &submitted.bundle_digest,
            record.fence,
            SchemaVerificationReceipt {
                id: "receipt-file-scope".into(),
                verifier_version: "scoped-file-test/v1".into(),
                input_digest: format!("sha256:{}", "a".repeat(64)),
                passed: true,
            },
        )
        .await
        .expect("File verification finishes");
    service
        .activate(
            "default",
            &security,
            ActivateSchemaBundleRequestV1 {
                request_id: "activate-file-scope".into(),
                idempotency_key: "activate-file-scope".into(),
                scope: SchemaScopeV1 {
                    kind: "task".into(),
                    id: scope_id.into(),
                },
                bundle_digest: submitted.bundle_digest.clone(),
                expected_predecessor: None,
                expected_fence: verified.fence,
                verification_receipt_id: verified
                    .verification_receipt_id
                    .expect("File receipt id committed"),
                stream_descriptor_completion_receipt_id: None,
            },
        )
        .await
        .expect("File scope activates");
    pin(scope_id, &submitted.bundle_digest)
}

fn scoped_invocation(
    state: crate::state::ServerState,
    template: &ModuleInvocationAuthority,
    pin: SchemaExecutionPin,
) -> std::sync::Arc<ApplicationDataInvocation> {
    ApplicationDataInvocation::new(
        state,
        ModuleInvocationAuthority::new(
            template.tenant.clone(),
            template.module_name.clone(),
            template.artifact_digest.clone(),
            template.trigger.clone(),
            template.triggering_entity_type.clone(),
            template.security.clone(),
            template.binding.clone(),
            ModuleDataTarget::Scoped(pin),
        ),
    )
}

fn entity_value(response: &temper_wasm_sdk::data::DataResponseV1) -> serde_json::Value {
    let DataOutcomeV1::Ok {
        result: DataResultV1::Entity { value, .. },
    } = &response.outcome
    else {
        panic!("expected entity result, got {response:?}")
    };
    serde_json::Value::Object(value.clone())
}

#[tokio::test]
async fn typed_module_data_is_isolated_by_exact_scope_and_bundle() {
    let operations = BTreeSet::from([
        DataOperationKind::EntityCreate,
        DataOperationKind::EntityGet,
        DataOperationKind::EntityPatch,
        DataOperationKind::EntityQuery,
        DataOperationKind::ActionInvoke,
    ]);
    let template = invocation(operations, SecurityContext::system());
    let mut state = template.state.clone();
    state.set_storage_stack(crate::storage::StorageStack::from_sim(
        temper_store_sim::SimEventStore::no_faults(7601),
        None,
    ));
    let pin_a = install_scope(&state, "task-a").await;
    let pin_b = install_scope(&state, "task-b").await;
    let scope_a = scoped_invocation(state.clone(), &template.authority, pin_a.clone());
    let scope_b = scoped_invocation(state.clone(), &template.authority, pin_b.clone());
    let global = ApplicationDataInvocation::new(state.clone(), template.authority.clone());
    let id = "018f1f80-7b2d-7000-8000-000000000076";

    for (invocation, name) in [(&scope_a, "scope-a"), (&scope_b, "scope-b")] {
        let created = call(
            invocation,
            DataOperationV1::EntityCreate {
                entity_type: "Temper.Example.Customer".into(),
                value: serde_json::json!({"Id": id, "Name": name})
                    .as_object()
                    .cloned()
                    .unwrap(),
            },
        )
        .await;
        assert!(
            matches!(
                created.outcome,
                DataOutcomeV1::Ok {
                    result: DataResultV1::Write { .. }
                }
            ),
            "scoped create failed: {created:?}"
        );
    }

    let read_a = call(
        &scope_a,
        DataOperationV1::EntityGet {
            entity_type: "Temper.Example.Customer".into(),
            entity_id: id.into(),
            at_least_sequence: None,
        },
    )
    .await;
    let read_b = call(
        &scope_b,
        DataOperationV1::EntityGet {
            entity_type: "Temper.Example.Customer".into(),
            entity_id: id.into(),
            at_least_sequence: None,
        },
    )
    .await;
    assert_eq!(entity_value(&read_a)["Name"], "scope-a");
    assert_eq!(entity_value(&read_b)["Name"], "scope-b");

    let patched = call(
        &scope_a,
        DataOperationV1::EntityPatch {
            entity_type: "Temper.Example.Customer".into(),
            entity_id: id.into(),
            expected_sequence: None,
            value: serde_json::json!({"Name": "scope-a-patched"})
                .as_object()
                .cloned()
                .unwrap(),
        },
    )
    .await;
    assert!(matches!(
        patched.outcome,
        DataOutcomeV1::Ok {
            result: DataResultV1::Write { .. }
        }
    ));
    let renamed = call(
        &scope_b,
        DataOperationV1::ActionInvoke {
            entity_type: "Temper.Example.Customer".into(),
            entity_id: id.into(),
            action: "Rename".into(),
            expected_sequence: None,
            params: serde_json::json!({"Name": "scope-b-renamed"})
                .as_object()
                .cloned()
                .unwrap(),
        },
    )
    .await;
    assert!(matches!(
        renamed.outcome,
        DataOutcomeV1::Ok {
            result: DataResultV1::Action { .. }
        }
    ));

    for current in [&pin_a, &pin_b] {
        state.stop_and_remove_scoped_entity(&template.authority.tenant, "Customer", id, current);
    }

    for (invocation, expected_name) in
        [(&scope_a, "scope-a-patched"), (&scope_b, "scope-b-renamed")]
    {
        let recovered = call(
            invocation,
            DataOperationV1::EntityGet {
                entity_type: "Temper.Example.Customer".into(),
                entity_id: id.into(),
                at_least_sequence: None,
            },
        )
        .await;
        assert_eq!(entity_value(&recovered)["Name"], expected_name);
        let page = call(
            invocation,
            DataOperationV1::EntityQuery {
                entity_type: "Temper.Example.Customer".into(),
                filter: None,
                order_by: Vec::new(),
                page: PageV1 {
                    limit: 10,
                    cursor: None,
                },
            },
        )
        .await;
        let DataOutcomeV1::Ok {
            result: DataResultV1::Page { values, .. },
        } = page.outcome
        else {
            panic!("expected scoped page")
        };
        assert_eq!(values.len(), 1);
        assert_eq!(values[0].value["Name"], expected_name);
    }

    let global_read = call(
        &global,
        DataOperationV1::EntityGet {
            entity_type: "Temper.Example.Customer".into(),
            entity_id: id.into(),
            at_least_sequence: None,
        },
    )
    .await;
    assert_eq!(
        response_error(global_read).kind,
        ModuleDataErrorKind::NotFound,
        "scoped writes must never leak into tenant-global application data"
    );

    let wrong_digest = scoped_invocation(
        state,
        &template.authority,
        pin("task-a", &format!("sha256:{}", "c".repeat(64))),
    );
    let missing = call(
        &wrong_digest,
        DataOperationV1::EntityGet {
            entity_type: "Temper.Example.Customer".into(),
            entity_id: id.into(),
            at_least_sequence: None,
        },
    )
    .await;
    assert_eq!(
        response_error(missing).kind,
        ModuleDataErrorKind::SchemaMismatch,
        "a missing exact bundle must fail instead of falling back"
    );
}

#[tokio::test]
async fn scoped_composite_uses_the_exact_pinned_actor() {
    let operations = BTreeSet::from([
        DataOperationKind::CompositeInvoke,
        DataOperationKind::EntityCreate,
        DataOperationKind::EntityGet,
    ]);
    let template = invocation(operations, SecurityContext::system());
    let mut state = template.state.clone();
    state.set_storage_stack(crate::storage::StorageStack::from_sim(
        temper_store_sim::SimEventStore::no_faults(7602),
        None,
    ));
    let pin = install_scope(&state, "composite-scope").await;
    let mut binding = template.authority.binding.clone();
    binding.grant.entities[0].actions.remove("Rename");
    binding.grant.entities[0]
        .composite_actions
        .insert("Rename".into());
    binding.grant_digest = binding.grant.digest().expect("composite grant digest");
    let scoped = ApplicationDataInvocation::new(
        state.clone(),
        ModuleInvocationAuthority::new(
            template.authority.tenant.clone(),
            template.authority.module_name.clone(),
            template.authority.artifact_digest.clone(),
            template.authority.trigger.clone(),
            template.authority.triggering_entity_type.clone(),
            template.authority.security.clone(),
            binding,
            ModuleDataTarget::Scoped(pin.clone()),
        ),
    );
    let id = "018f1f80-7b2d-7000-8000-000000000077";
    let created = call(
        &scoped,
        DataOperationV1::EntityCreate {
            entity_type: "Temper.Example.Customer".into(),
            value: serde_json::json!({"Id": id, "Name": "before"})
                .as_object()
                .cloned()
                .unwrap(),
        },
    )
    .await;
    assert!(
        matches!(created.outcome, DataOutcomeV1::Ok { .. }),
        "scoped composite create failed: {created:?}"
    );
    let composite = call(
        &scoped,
        DataOperationV1::CompositeInvoke {
            entity_type: "Temper.Example.Customer".into(),
            entity_id: id.into(),
            action: "Rename".into(),
            expected_sequence: None,
            params: serde_json::json!({"Name": "after"})
                .as_object()
                .cloned()
                .unwrap(),
        },
    )
    .await;
    assert!(matches!(
        composite.outcome,
        DataOutcomeV1::Ok {
            result: DataResultV1::Action { .. }
        }
    ));
    let scoped_state = state
        .get_scoped_entity_state(&template.authority.tenant, "Customer", id, pin)
        .await
        .expect("composite target should remain in the pinned journal");
    assert_eq!(scoped_state.state.fields["Name"], "after");
    assert!(!state.entity_exists(&template.authority.tenant, "Customer", id));
}

#[tokio::test]
async fn scoped_cedar_denial_preserves_stable_error_fields() {
    let security = SecurityContext::from_resolved_identity("scoped-user", "test-agent", None);
    let template = invocation(BTreeSet::from([DataOperationKind::EntityCreate]), security);
    let mut state = template.state.clone();
    state.set_storage_stack(crate::storage::StorageStack::from_sim(
        temper_store_sim::SimEventStore::no_faults(7603),
        None,
    ));
    let pin = install_scope(&state, "cedar-scope").await;
    state
        .authz
        .reload_tenant_policies_named(
            template.authority.tenant.as_str(),
            &[
                (
                    "allow-scoped-customer".to_string(),
                    r#"permit(principal, action, resource is Customer);"#.to_string(),
                ),
                (
                    "decision:block-scoped-create".to_string(),
                    r#"forbid(principal, action == Action::"create", resource is Customer);"#
                        .to_string(),
                ),
            ],
        )
        .expect("restrictive scoped policy should parse");
    let scoped = scoped_invocation(state, &template.authority, pin);
    let denied = call(
        &scoped,
        DataOperationV1::EntityCreate {
            entity_type: "Temper.Example.Customer".into(),
            value: serde_json::json!({"Id":"018f1f80-7b2d-7000-8000-000000000078"})
                .as_object()
                .cloned()
                .unwrap(),
        },
    )
    .await;
    let error = response_error(denied);
    assert_eq!(error.kind, ModuleDataErrorKind::AuthorizationDenied);
    assert_eq!(error.code, "AuthorizationDenied");
    assert_eq!(error.message, "caller is not authorized for this operation");
    assert_eq!(error.retryability, Retryability::Never);
    assert!(
        error
            .decision_id
            .as_deref()
            .is_some_and(|id| id.contains("decision:block-scoped-create"))
    );
    let details = error.details.expect("Cedar denial details survive the ABI");
    assert_eq!(details["denial_class"], "policy_denied");
    assert!(
        details["policy_ids"]
            .as_array()
            .is_some_and(|ids| ids.iter().any(|id| {
                id.as_str()
                    .is_some_and(|id| id.contains("decision:block-scoped-create"))
            }))
    );
}

#[tokio::test]
async fn scoped_call_budget_is_enforced_without_losing_error_shape() {
    let template = invocation(
        BTreeSet::from([
            DataOperationKind::EntityCreate,
            DataOperationKind::EntityGet,
        ]),
        SecurityContext::system(),
    );
    let mut state = template.state.clone();
    state.set_storage_stack(crate::storage::StorageStack::from_sim(
        temper_store_sim::SimEventStore::no_faults(7604),
        None,
    ));
    let pin = install_scope(&state, "budget-scope").await;
    let mut binding = template.authority.binding.clone();
    binding.grant.budgets.max_calls = 1;
    binding.grant_digest = binding.grant.digest().expect("budgeted grant digest");
    let scoped = ApplicationDataInvocation::new(
        state,
        ModuleInvocationAuthority::new(
            template.authority.tenant.clone(),
            template.authority.module_name.clone(),
            template.authority.artifact_digest.clone(),
            template.authority.trigger.clone(),
            template.authority.triggering_entity_type.clone(),
            template.authority.security.clone(),
            binding,
            ModuleDataTarget::Scoped(pin),
        ),
    );
    let id = "018f1f80-7b2d-7000-8000-000000000079";
    let first = call(
        &scoped,
        DataOperationV1::EntityCreate {
            entity_type: "Temper.Example.Customer".into(),
            value: serde_json::json!({"Id": id}).as_object().cloned().unwrap(),
        },
    )
    .await;
    assert!(matches!(first.outcome, DataOutcomeV1::Ok { .. }));
    let exhausted = call(
        &scoped,
        DataOperationV1::EntityGet {
            entity_type: "Temper.Example.Customer".into(),
            entity_id: id.into(),
            at_least_sequence: None,
        },
    )
    .await;
    let error = response_error(exhausted);
    assert_eq!(error.kind, ModuleDataErrorKind::BudgetExceeded);
    assert_eq!(error.code, "CallBudgetExceeded");
    assert_eq!(error.retryability, Retryability::Never);
}

#[tokio::test]
async fn scoped_query_rejects_an_authoritative_set_larger_than_its_scan_budget() {
    let template = invocation(
        BTreeSet::from([
            DataOperationKind::EntityCreate,
            DataOperationKind::EntityQuery,
        ]),
        SecurityContext::system(),
    );
    let mut state = template.state.clone();
    state.set_storage_stack(crate::storage::StorageStack::from_sim(
        temper_store_sim::SimEventStore::no_faults(7607),
        None,
    ));
    let pin = install_scope(&state, "query-budget-scope").await;
    let mut binding = template.authority.binding.clone();
    binding.grant.budgets.max_page_items = 1;
    binding.grant_digest = binding.grant.digest().expect("query grant digest");
    let scoped = ApplicationDataInvocation::new(
        state,
        ModuleInvocationAuthority::new(
            template.authority.tenant.clone(),
            template.authority.module_name.clone(),
            template.authority.artifact_digest.clone(),
            template.authority.trigger.clone(),
            template.authority.triggering_entity_type.clone(),
            template.authority.security.clone(),
            binding,
            ModuleDataTarget::Scoped(pin),
        ),
    );
    for suffix in 1..=9 {
        let created = call(
            &scoped,
            DataOperationV1::EntityCreate {
                entity_type: "Temper.Example.Customer".into(),
                value: serde_json::json!({
                    "Id": format!("018f1f80-7b2d-7000-8000-{suffix:012}"),
                    "Name": format!("customer-{suffix}")
                })
                .as_object()
                .cloned()
                .unwrap(),
            },
        )
        .await;
        assert!(
            matches!(created.outcome, DataOutcomeV1::Ok { .. }),
            "scoped query fixture create failed: {created:?}"
        );
    }
    let response = call(
        &scoped,
        DataOperationV1::EntityQuery {
            entity_type: "Temper.Example.Customer".into(),
            filter: None,
            order_by: Vec::new(),
            page: PageV1 {
                limit: 1,
                cursor: None,
            },
        },
    )
    .await;
    let error = response_error(response);
    assert_eq!(error.kind, ModuleDataErrorKind::BudgetExceeded);
    assert_eq!(error.code, "QueryFallbackBudgetExceeded");
}

#[tokio::test]
async fn scoped_binding_cannot_cross_tenant_even_with_the_same_pin() {
    let template = invocation(
        BTreeSet::from([DataOperationKind::EntityCreate]),
        SecurityContext::system(),
    );
    let mut state = template.state.clone();
    state.set_storage_stack(crate::storage::StorageStack::from_sim(
        temper_store_sim::SimEventStore::no_faults(7605),
        None,
    ));
    let pin = install_scope(&state, "tenant-scope").await;
    let other = ApplicationDataInvocation::new(
        state,
        ModuleInvocationAuthority::new(
            temper_runtime::tenant::TenantId::new("other-tenant"),
            template.authority.module_name.clone(),
            template.authority.artifact_digest.clone(),
            template.authority.trigger.clone(),
            template.authority.triggering_entity_type.clone(),
            template.authority.security.clone(),
            template.authority.binding.clone(),
            ModuleDataTarget::Scoped(pin),
        ),
    );
    let denied = call(
        &other,
        DataOperationV1::EntityCreate {
            entity_type: "Temper.Example.Customer".into(),
            value: serde_json::json!({"Id":"018f1f80-7b2d-7000-8000-000000000080"})
                .as_object()
                .cloned()
                .unwrap(),
        },
    )
    .await;
    let error = response_error(denied);
    assert_eq!(error.kind, ModuleDataErrorKind::SchemaMismatch);
    assert_eq!(error.code, "ScopedSchemaUnavailable");
}

#[tokio::test]
async fn scoped_native_file_write_commits_only_to_the_pinned_journal() {
    let sim = temper_store_sim::SimEventStore::no_faults(7606);
    let mut state = crate::state::ServerState::from_registry(
        temper_runtime::ActorSystem::new("scoped-file-write"),
        crate::registry::SpecRegistry::new(),
    );
    state.set_storage_stack(crate::storage::StorageStack::from_sim(sim, None));
    let data_dir = tempfile::tempdir().expect("scoped File blob directory");
    state.data_dir = data_dir.path().to_path_buf();
    let pin = install_file_scope(&state).await;
    let grant = ModuleDataGrant {
        operations: BTreeSet::from([
            DataOperationKind::EntityCreate,
            DataOperationKind::FileWrite,
        ]),
        entities: vec![EntityDataGrant {
            entity_type: "Temper.ScopedFile.File".into(),
            file_operations: BTreeSet::from([FileOperationKind::ContentWrite]),
            ..EntityDataGrant::default()
        }],
        ..ModuleDataGrant::default()
    };
    let generated = temper_codegen::generate_module_sdk(
        &temper_spec::parse_csdl(FILE_CSDL).expect("File CSDL parses"),
        &[IoaSourceInput {
            entity_type: "Temper.ScopedFile.File".into(),
            source: FILE_IOA.into(),
        }],
        "file-worker",
        "file-closure",
        "file-closure",
        "file-artifact",
        grant,
    )
    .expect("File client binding generates");
    let invocation = ApplicationDataInvocation::new(
        state.clone(),
        ModuleInvocationAuthority::new(
            temper_runtime::tenant::TenantId::default(),
            "file-worker".into(),
            "file-artifact".into(),
            "Write".into(),
            "File".into(),
            SecurityContext::system(),
            generated.manifest,
            ModuleDataTarget::Scoped(pin.clone()),
        ),
    );
    let missing_workspace_file = "scoped-file-missing-workspace";
    let created = call(
        &invocation,
        DataOperationV1::EntityCreate {
            entity_type: "Temper.ScopedFile.File".into(),
            value: serde_json::json!({
                "Id": missing_workspace_file,
                "Status": "Created",
                "workspace_id": "missing-workspace"
            })
            .as_object()
            .cloned()
            .unwrap(),
        },
    )
    .await;
    assert!(
        matches!(created.outcome, DataOutcomeV1::Ok { .. }),
        "scoped missing-Workspace File create failed: {created:?}"
    );
    let blob_entries_before = std::fs::read_dir(data_dir.path())
        .expect("scoped File blob directory remains readable")
        .count();
    let opened = call(
        &invocation,
        DataOperationV1::FileWriteOpen {
            file_id: missing_workspace_file.into(),
            expected_sequence: None,
            content_length: Some(7),
            content_hash: None,
        },
    )
    .await;
    let DataOutcomeV1::Ok {
        result: DataResultV1::FileWrite { stream_handle },
    } = opened.outcome
    else {
        panic!("scoped missing-Workspace File stream should open: {opened:?}")
    };
    assert_eq!(invocation.stream_write(stream_handle, b"blocked"), Ok(7));
    let rejected = call(
        &invocation,
        DataOperationV1::FileWriteCommit { stream_handle },
    )
    .await;
    assert!(
        matches!(rejected.outcome, DataOutcomeV1::Error { .. }),
        "missing scoped Workspace must fail closed: {rejected:?}"
    );
    assert_eq!(
        std::fs::read_dir(data_dir.path())
            .expect("scoped File blob directory remains readable")
            .count(),
        blob_entries_before,
        "a failed scoped Workspace lookup must not persist blob bytes"
    );
    let rejected_file = state
        .get_scoped_entity_state(
            &temper_runtime::tenant::TenantId::default(),
            "File",
            missing_workspace_file,
            pin.clone(),
        )
        .await
        .expect("rejected File remains readable in its exact scope");
    assert_eq!(rejected_file.state.status, "Created");

    let file_id = "scoped-file";
    let created = call(
        &invocation,
        DataOperationV1::EntityCreate {
            entity_type: "Temper.ScopedFile.File".into(),
            value: serde_json::json!({"Id": file_id, "Status": "Created"})
                .as_object()
                .cloned()
                .unwrap(),
        },
    )
    .await;
    assert!(
        matches!(created.outcome, DataOutcomeV1::Ok { .. }),
        "scoped native File create failed: {created:?}"
    );
    let opened = call(
        &invocation,
        DataOperationV1::FileWriteOpen {
            file_id: file_id.into(),
            expected_sequence: None,
            content_length: Some(6),
            content_hash: None,
        },
    )
    .await;
    let DataOutcomeV1::Ok {
        result: DataResultV1::FileWrite { stream_handle },
    } = opened.outcome
    else {
        panic!("scoped File stream should open: {opened:?}")
    };
    assert_eq!(invocation.stream_write(stream_handle, b"scoped"), Ok(6));
    let committed = call(
        &invocation,
        DataOperationV1::FileWriteCommit { stream_handle },
    )
    .await;
    assert!(matches!(
        committed.outcome,
        DataOutcomeV1::Ok {
            result: DataResultV1::FileCommitted { .. }
        }
    ));
    let file = state
        .get_scoped_entity_state(
            &temper_runtime::tenant::TenantId::default(),
            "File",
            file_id,
            pin,
        )
        .await
        .expect("scoped File should be readable at its exact pin");
    assert_eq!(file.state.status, "Ready");
    assert!(!state.entity_exists(
        &temper_runtime::tenant::TenantId::default(),
        "File",
        file_id
    ));
}

#[tokio::test]
async fn seeded_scoped_restart_and_fault_schedules_preserve_isolation() {
    let operations = BTreeSet::from([
        DataOperationKind::EntityCreate,
        DataOperationKind::EntityGet,
        DataOperationKind::EntityPatch,
    ]);
    let id = "018f1f80-7b2d-7000-8000-000000000081";
    for seed in 7_610..7_618 {
        let sim = temper_store_sim::SimEventStore::no_faults(seed);
        let template = invocation(operations.clone(), SecurityContext::system());
        let mut state = template.state.clone();
        state.set_storage_stack(crate::storage::StorageStack::from_sim(sim.clone(), None));
        let pin_a = install_scope(&state, &format!("seed-{seed}-a")).await;
        let pin_b = install_scope(&state, &format!("seed-{seed}-b")).await;
        let scope_a = scoped_invocation(state.clone(), &template.authority, pin_a.clone());
        let scope_b = scoped_invocation(state.clone(), &template.authority, pin_b.clone());
        for (current, name) in [(&scope_a, "scope-a"), (&scope_b, "scope-b")] {
            let created = call(
                current,
                DataOperationV1::EntityCreate {
                    entity_type: "Temper.Example.Customer".into(),
                    value: serde_json::json!({"Id": id, "Name": name})
                        .as_object()
                        .cloned()
                        .unwrap(),
                },
            )
            .await;
            assert!(
                matches!(created.outcome, DataOutcomeV1::Ok { .. }),
                "seed {seed} failed initial scoped create: {created:?}"
            );
        }

        drop(scope_a);
        drop(scope_b);
        drop(state);
        let restart_template = invocation(operations.clone(), SecurityContext::system());
        let mut restarted = restart_template.state.clone();
        restarted.set_storage_stack(crate::storage::StorageStack::from_sim(sim.clone(), None));
        let deployment = crate::schema_deployment::GovernedSchemaDeploymentService::new(&restarted);
        for pin in [&pin_a, &pin_b] {
            deployment
                .recover_registry_pointer(
                    temper_runtime::tenant::TenantId::default().as_str(),
                    &pin.scope,
                )
                .await
                .unwrap_or_else(|error| panic!("seed {seed} registry recovery failed: {error:?}"));
        }
        let recovered_a = scoped_invocation(
            restarted.clone(),
            &restart_template.authority,
            pin_a.clone(),
        );
        let recovered_b = scoped_invocation(
            restarted.clone(),
            &restart_template.authority,
            pin_b.clone(),
        );
        let mut rng = temper_store_sim::DeterministicRng::new(seed);
        let target_a = rng.next_u64() & 1 == 0;
        let violation_count = 1 + rng.next_u64() % 2;
        let (target, target_pin, expected_target, other, expected_other) = if target_a {
            (
                &recovered_a,
                &pin_a,
                "scope-a".to_string(),
                &recovered_b,
                "scope-b".to_string(),
            )
        } else {
            (
                &recovered_b,
                &pin_b,
                "scope-b".to_string(),
                &recovered_a,
                "scope-a".to_string(),
            )
        };
        let journal_id = temper_runtime::persistence::schema_deployment::scoped_journal_entity_id(
            id, target_pin,
        );
        let persistence_id = format!("default:Customer:{journal_id}");
        sim.inject_concurrency_violations(&persistence_id, violation_count);
        assert_eq!(
            sim.pending_concurrency_violations(&persistence_id),
            violation_count
        );
        let updated_name = format!("{expected_target}-updated-{violation_count}");
        let mut updated = false;
        for _ in 0..=violation_count {
            let response = call(
                target,
                DataOperationV1::EntityPatch {
                    entity_type: "Temper.Example.Customer".into(),
                    entity_id: id.into(),
                    expected_sequence: None,
                    value: serde_json::json!({"Name": updated_name.clone()})
                        .as_object()
                        .cloned()
                        .unwrap(),
                },
            )
            .await;
            if matches!(response.outcome, DataOutcomeV1::Ok { .. }) {
                updated = true;
                break;
            }
        }
        assert!(updated, "seed {seed} exhausted its scoped retry budget");
        assert_eq!(sim.pending_concurrency_violations(&persistence_id), 0);
        for (current, expected) in [
            (target, updated_name.as_str()),
            (other, expected_other.as_str()),
        ] {
            let response = call(
                current,
                DataOperationV1::EntityGet {
                    entity_type: "Temper.Example.Customer".into(),
                    entity_id: id.into(),
                    at_least_sequence: None,
                },
            )
            .await;
            assert_eq!(
                entity_value(&response)["Name"],
                expected,
                "seed {seed} crossed an immutable scope boundary"
            );
        }
    }
}
