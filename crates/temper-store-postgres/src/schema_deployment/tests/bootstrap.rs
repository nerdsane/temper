use super::*;

use temper_runtime::persistence::schema_deployment::{
    CompleteSchemaBootstrap, RecordSchemaBootstrapCreated, ReserveSchemaBootstrap,
    ReserveSchemaBootstrapOutcome, SchemaBootstrapReceipt, SchemaBootstrapStatus,
};

fn bootstrap_command(
    tenant: &str,
    activation_request_id: &str,
    caller_hex: char,
    idempotency_key: &str,
) -> ReserveSchemaBootstrap {
    ReserveSchemaBootstrap {
        tenant: tenant.into(),
        caller_authority: format!("sha256:{}", caller_hex.to_string().repeat(64)),
        accepted_authority_json: r#"{"principal":"postgres-contract"}"#.into(),
        idempotency_key: idempotency_key.into(),
        request_digest: format!("sha256:{}", caller_hex.to_string().repeat(64)),
        request_id: format!("request-{idempotency_key}"),
        activation_request_id: activation_request_id.into(),
        entity_type: "Example.Task".into(),
        entity_id: "bootstrap-target".into(),
        canonical_initial_fields_json: r#"{"Title":"first"}"#.into(),
        initial_action: None,
    }
}

#[test]
fn postgres_bootstrap_coordinator_reserves_target_and_replays_exact_receipt() {
    let database_url = match std::env::var("DATABASE_URL") {
        Ok(url) => url,
        Err(_) => {
            eprintln!("skipping Postgres bootstrap test: DATABASE_URL is not set");
            return;
        }
    };

    sqlx::test_block_on(async {
        let pool = PgPool::connect(&database_url)
            .await
            .expect("connect to DATABASE_URL");
        run_migrations(&pool).await.expect("run migrations");
        let store = PostgresEventStore::new(pool);
        let suffix = uuid::Uuid::new_v4();
        let tenant = format!("schema-bootstrap-{suffix}");
        let test_scope = scope(&format!("task-{suffix}"));
        let digest = format!("sha256:{}", "6".repeat(64));
        let request_digest = format!("sha256:{}", "7".repeat(64));
        store
            .submit_schema_bundle(submission(
                &tenant,
                &test_scope,
                "bootstrap-submit",
                &request_digest,
                &digest,
            ))
            .await
            .expect("submit bootstrap bundle");
        let verified = verify(
            &store,
            &tenant,
            &test_scope,
            &digest,
            &request_digest,
            "bootstrap-verification",
        )
        .await;
        let pointer = activated(
            store
                .activate_schema_bundle(ActivateSchemaBundle {
                    tenant: tenant.clone(),
                    scope: test_scope,
                    bundle_digest: digest,
                    expected_predecessor: None,
                    expected_fence: verified.fence,
                    verification_receipt_id: "bootstrap-verification".into(),
                    stream_publication_fence: None,
                    operation: operation("bootstrap-activate"),
                })
                .await
                .expect("activate bootstrap bundle"),
        );
        let command = bootstrap_command(
            &tenant,
            &pointer.accepted_request_id,
            'a',
            "bootstrap-operation",
        );
        let reserved = match store
            .reserve_schema_bootstrap(command.clone())
            .await
            .expect("reserve bootstrap")
        {
            ReserveSchemaBootstrapOutcome::Reserved(operation) => operation,
            ReserveSchemaBootstrapOutcome::Replayed(_) => panic!("first reservation must be new"),
        };
        let conflicting_target =
            bootstrap_command(&tenant, &pointer.accepted_request_id, 'b', "bootstrap-race");
        assert_eq!(
            store
                .reserve_schema_bootstrap(conflicting_target)
                .await
                .expect_err("a second operation must not acquire the target"),
            SchemaDeploymentStoreError::BootstrapTargetConflict
        );
        let created = store
            .record_schema_bootstrap_created(RecordSchemaBootstrapCreated {
                tenant: tenant.clone(),
                caller_authority: command.caller_authority.clone(),
                idempotency_key: command.idempotency_key.clone(),
                expected_sequence: reserved.committed_sequence,
                creation_sequence: 1,
            })
            .await
            .expect("record bootstrap creation");
        let receipt = SchemaBootstrapReceipt {
            request_id: command.request_id.clone(),
            pin: created.pin.clone(),
            entity_type: command.entity_type.clone(),
            entity_id: command.entity_id.clone(),
            creation_sequence: Some(1),
            action_sequence: None,
            canonical_action_result_json: None,
            failure: None,
        };
        let completed = store
            .complete_schema_bootstrap(CompleteSchemaBootstrap {
                tenant: tenant.clone(),
                caller_authority: command.caller_authority.clone(),
                idempotency_key: command.idempotency_key.clone(),
                expected_sequence: created.committed_sequence,
                receipt: receipt.clone(),
            })
            .await
            .expect("complete bootstrap");
        assert_eq!(completed.status, SchemaBootstrapStatus::Completed);
        let replay = match store
            .reserve_schema_bootstrap(command)
            .await
            .expect("replay completed bootstrap")
        {
            ReserveSchemaBootstrapOutcome::Replayed(operation) => operation,
            ReserveSchemaBootstrapOutcome::Reserved(_) => panic!("completed retry must replay"),
        };
        assert_eq!(replay.receipt.as_ref(), Some(&receipt));
    });
}
