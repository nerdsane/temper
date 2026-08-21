mod common;

use common::reaction_fixture::*;
use temper_runtime::persistence::schema_deployment::{
    ActivateSchemaBundle, ClaimSchemaVerification, ClaimSchemaVerificationOutcome,
    RetireSchemaBundle, SchemaBundleRecord, SchemaDeploymentStore, SchemaExecutionPin,
    SchemaOperationIdentity, SchemaScope, SchemaScopeKind, SchemaVerificationReceipt,
    SubmitSchemaBundle,
};
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

fn scoped_state(
    tenant: &TenantId,
    scope: &SchemaScope,
    digest: &str,
    store: SimEventStore,
) -> ServerState {
    let mut registry = SpecRegistry::new();
    registry
        .stage_scoped_bundle(
            tenant.clone(),
            scope.clone(),
            digest.to_string(),
            parse_csdl(CSDL_XML).expect("CSDL should parse"),
            CSDL_XML.to_string(),
            &[("Order", SCOPED_ORDER_IOA), ("Payment", PAYMENT_IOA)],
        )
        .expect("scoped bundle should stage");
    registry
        .activate_scoped_bundle(tenant, scope, digest, None)
        .expect("scoped bundle should activate");
    let mut state = ServerState::from_registry(ActorSystem::new("scoped-reaction"), registry);
    state.set_storage_stack(StorageStack::from_sim(store, None));
    state.rebuild_reaction_dispatcher();
    state
}

async fn activate_durable_pin(tenant: &TenantId, pin: &SchemaExecutionPin, store: &SimEventStore) {
    let submitted = store
        .submit_schema_bundle(SubmitSchemaBundle {
            bundle: SchemaBundleRecord {
                tenant: tenant.to_string(),
                scope: pin.scope.clone(),
                digest: pin.bundle_digest.clone(),
                predecessor_digest: None,
                canonical_csdl: CSDL_XML.into(),
                canonical_ioa: std::collections::BTreeMap::from([
                    ("Order".into(), SCOPED_ORDER_IOA.into()),
                    ("Payment".into(), PAYMENT_IOA.into()),
                ]),
                cedar_policies: std::collections::BTreeMap::new(),
                wasm_module_digests: std::collections::BTreeMap::new(),
                migration_module_name: None,
                migration_module_digest: None,
                migration_abi_version: None,
                canonical_budgets: "{}".into(),
            },
            idempotency_key: "submit-scoped-reaction".into(),
            request_digest: format!("sha256:{}", "1".repeat(64)),
            request_id: "submit-scoped-reaction".into(),
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
                idempotency_key: "verify-scoped-reaction".into(),
                request_digest: format!("sha256:{}", "2".repeat(64)),
                request_id: "verify-scoped-reaction".into(),
            },
        })
        .await
        .expect("durable bundle should claim verification");
    let fence = match claimed {
        ClaimSchemaVerificationOutcome::Claimed(record)
        | ClaimSchemaVerificationOutcome::Replayed(record) => record.fence,
    };
    let verified = store
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
        .expect("durable bundle should verify");
    store
        .activate_schema_bundle(ActivateSchemaBundle {
            tenant: tenant.to_string(),
            scope: pin.scope.clone(),
            bundle_digest: digest,
            expected_predecessor: None,
            expected_fence: verified.fence,
            verification_receipt_id: "scoped-reaction-verification".into(),
            operation: SchemaOperationIdentity {
                idempotency_key: "activate-scoped-reaction".into(),
                request_digest: format!("sha256:{}", "4".repeat(64)),
                request_id: "activate-scoped-reaction".into(),
            },
        })
        .await
        .expect("durable bundle should activate");
}

#[tokio::test]
async fn scoped_durable_reaction_materializes_and_reconciles_at_exact_pin() {
    let (_guard, _clock, _ids) = install_deterministic_context(914);
    let tenant = TenantId::new("scoped-reaction-tenant");
    let scope = SchemaScope {
        kind: SchemaScopeKind::Task,
        id: "task-914".into(),
    };
    let digest = format!("sha256:{}", "9".repeat(64));
    let pin = SchemaExecutionPin {
        scope: scope.clone(),
        bundle_digest: digest.clone(),
    };
    let store = SimEventStore::no_faults(914);
    activate_durable_pin(&tenant, &pin, &store).await;
    let state = scoped_state(&tenant, &scope, &digest, store.clone());
    let context = AgentContext {
        schema_pin: Some(pin.clone()),
        ..AgentContext::default()
    };
    state
        .dispatch_tenant_action(
            &tenant,
            "Order",
            "order-1",
            "ConfirmOrder",
            serde_json::json!({}),
            &context,
        )
        .await
        .expect("scoped source action should dispatch");

    let source_id = format!(
        "{tenant}:Order:{}",
        temper_runtime::persistence::schema_deployment::scoped_journal_entity_id("order-1", &pin,)
    );
    let source = store.dump_journal(&source_id);
    let intent = extract_intents(
        &source
            .iter()
            .find(|event| event.event_type == "ConfirmOrder")
            .expect("source event should exist")
            .payload,
    )
    .expect("source intent should decode")
    .pop()
    .expect("source intent should exist");
    assert_eq!(
        intent.schema_pin.as_ref().map(|value| &value.execution),
        Some(&pin)
    );

    let lifecycle_id = delivery_journal_id(&intent);
    let lifecycle = store.dump_journal(&lifecycle_id);
    let mut ambiguous: ReactionDeliveryRecord = serde_json::from_value(
        lifecycle
            .last()
            .expect("delivery lifecycle should exist")
            .payload
            .clone(),
    )
    .expect("delivery lifecycle should decode");
    ambiguous.status = ReactionDeliveryStatus::Dispatching;
    ambiguous.lease_expires_at = Some(sim_now() - chrono::Duration::seconds(1));
    append_delivery_record(
        &BoxedEventStore::new(store.clone()),
        lifecycle
            .last()
            .expect("delivery sequence should exist")
            .sequence_nr,
        &ambiguous,
    )
    .await
    .expect("ambiguous response-loss state should persist");
    drop(state);

    let active = store
        .active_schema_pointer(tenant.as_str(), &scope)
        .await
        .expect("active pointer lookup should succeed")
        .expect("active pointer should exist");
    store
        .retire_schema_bundle(RetireSchemaBundle {
            tenant: tenant.to_string(),
            scope: scope.clone(),
            bundle_digest: digest.clone(),
            expected_fence: active.fence,
            operation: SchemaOperationIdentity {
                idempotency_key: "retire-scoped-reaction".into(),
                request_digest: format!("sha256:{}", "5".repeat(64)),
                request_id: "retire-scoped-reaction".into(),
            },
        })
        .await
        .expect("durable bundle should retire before recovery");

    let mut restarted = ServerState::from_registry(
        ActorSystem::new("scoped-reaction-restart"),
        SpecRegistry::new(),
    );
    restarted.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    restarted.rebuild_reaction_dispatcher();
    assert!(
        restarted
            .registry
            .read()
            .expect("registry lock")
            .get_scoped_config_at_digest(&tenant, &scope, &digest)
            .is_none(),
        "restart fixture must begin without manually hydrated scoped metadata"
    );
    let dispatcher = restarted
        .reaction_dispatcher
        .read()
        .expect("dispatcher lock")
        .clone()
        .expect("dispatcher should exist");
    dispatcher
        .dispatch_committed_intent(&restarted, intent)
        .await
        .expect("scoped receipt reconciliation should succeed");

    let target_id = format!(
        "{tenant}:Payment:{}",
        temper_runtime::persistence::schema_deployment::scoped_journal_entity_id("order-1", &pin,)
    );
    assert_eq!(
        store
            .dump_journal(&target_id)
            .iter()
            .filter(|event| event.event_type == "AuthorizePayment")
            .count(),
        1,
        "recovery must not duplicate the pinned target event"
    );
}
