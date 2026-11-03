//! Field-index backfill enumerates the durable store, not the lazy index.
use super::*;

/// Legacy directories without field index rows are in migration 0023's gap.
/// Equality reads reconcile them until backfill makes the lookup cost its match.
/// Gated on DATABASE_URL; unique tenant.
#[test]
fn field_index_backfill_bounds_non_keyed_path_lookup_on_postgres() {
    use temper_runtime::scheduler::sim_uuid as runtime_sim_uuid;
    let Some(database_url) = temper_test_resource::env_var("DATABASE_URL") else {
        return;
    };
    sqlx::test_block_on(async {
        let pool = sqlx::PgPool::connect(&database_url).await.unwrap();
        temper_store_postgres::migration::run_migrations(&pool)
            .await
            .unwrap();
        let store = temper_store_postgres::PostgresEventStore::new(pool.clone());
        let tenant = TenantId::from(format!("fieldidx-pg-{}", runtime_sim_uuid()));
        let csdl = parse_csdl(CSDL_XML).expect("CSDL parses");
        let mut registry = SpecRegistry::new();
        registry.register_tenant(
            tenant.as_str(),
            csdl,
            CSDL_XML.to_string(),
            &[("Order", ORDER_IOA), ("Directory", DIRECTORY_IOA)],
        );
        let mut state = ServerState::from_registry(ActorSystem::new("fieldidx-pg"), registry);
        state.set_storage_stack(StorageStack::from_postgres(store.clone()));
        let agent_ctx = AgentContext::for_service("fieldidx-pg-test");
        let security_ctx = SecurityContext::system();

        // root + /souls + padding (> budget). `/souls` is looked up by Path (non-keyed).
        pg_seed_dir(
            &state,
            &tenant,
            &agent_ctx,
            "fx-root",
            ("/", "/", "wsA", None),
        )
        .await;
        pg_seed_dir(
            &state,
            &tenant,
            &agent_ctx,
            "fx-souls",
            ("souls", "/souls", "wsA", Some("fx-root")),
        )
        .await;
        for i in 0..13 {
            pg_seed_dir(
                &state,
                &tenant,
                &agent_ctx,
                &format!("fx-sub-{i}"),
                (
                    &format!("sub{i}"),
                    &format!("/sub{i}"),
                    "wsA",
                    Some("fx-root"),
                ),
            )
            .await;
        }

        // Model a migrated legacy catalog: no field index or projection
        // fingerprint, and the unindexed rows recorded by migration 0023.
        // The lazy index is empty at a fresh boot, so backfill must enumerate
        // the durable journal to reach these entities.
        state.entity_index.write().unwrap().clear();
        state.entity_index_hydrated.write().unwrap().clear();
        let mut tx = pool.begin().await.unwrap();
        for sql in [
            "DELETE FROM entity_field_index WHERE tenant = $1",
            "UPDATE entity_catalog SET projection_hash = '' WHERE tenant = $1",
            "INSERT INTO entity_projection_delta (tenant, entity_type, entity_id, sequence_nr) \
             SELECT tenant, entity_type, entity_id, sequence_nr FROM entity_catalog WHERE tenant = $1 \
             ON CONFLICT (tenant, entity_type, entity_id) DO NOTHING",
        ] {
            sqlx::query(sql)
                .bind(tenant.as_str())
                .execute(&mut *tx)
                .await
                .unwrap();
        }
        tx.commit().await.unwrap();

        let budget = QueryPlaneReadBudget {
            default_page_size: 1,
            max_entities: 1,
        };
        // `Path eq '/souls' and WorkspaceId eq 'wsA'` — non-keyed; needs the field index.
        let eq = |p: &str, v: &str| FilterExpr::BinaryOp {
            left: Box::new(FilterExpr::Property(p.to_string())),
            op: BinaryOperator::Eq,
            right: Box::new(FilterExpr::Literal(ODataValue::String(v.to_string()))),
        };
        let qo = QueryOptions {
            filter: Some(FilterExpr::BinaryOp {
                left: Box::new(eq("Path", "/souls")),
                op: BinaryOperator::And,
                right: Box::new(eq("WorkspaceId", "wsA")),
            }),
            ..QueryOptions::default()
        };

        // Before backfill, the gap keeps the matching directory visible,
        // even though the complete gap exceeds this read's probe budget.
        let before = read_entity_set_page(QueryPlaneReadRequest {
            state: &state,
            tenant: &tenant,
            security_ctx: &security_ctx,
            entity_type: "Directory",
            entity_set_name: "Directories",
            query_options: &qo,
            budget,
        })
        .await
        .unwrap_or_else(|_| panic!("the migrated gap must keep unindexed directories visible"));
        assert_eq!(before.entities.len(), 1);
        assert_eq!(before.entities[0]["entity_id"], "fx-souls");
        assert_eq!(before.telemetry.coverage.missing, 15);
        assert_eq!(before.telemetry.materialized_count, 15);
        assert_eq!(
            before.telemetry.fallback_reason,
            QueryPlaneFallbackReason::ProjectionLagReconcile
        );

        // (2) Backfill must enumerate the registry and durable store. Using
        // the empty lazy index would leave every legacy directory pending.
        // The enumeration has NO declared-key branch (unlike the key-index backfill), so
        // it covers every registered type identically — keyed or not; this case exercises
        // a non-key FIELD (`Path`) on a keyed type, which is the field index's job.
        assert!(state.entity_index.read().unwrap().is_empty());
        state.populate_field_index_from_snapshots(&tenant).await;

        // (3) The same Path lookup now reads only its one match — proof
        // the backfill populated the field index for entities absent from the lazy index.
        let r = match read_entity_set_page(QueryPlaneReadRequest {
            state: &state,
            tenant: &tenant,
            security_ctx: &security_ctx,
            entity_type: "Directory",
            entity_set_name: "Directories",
            query_options: &qo,
            budget,
        })
        .await
        {
            Ok(r) => r,
            Err(_) => panic!("Path lookup must bind via the field index after the backfill"),
        };
        assert_eq!(
            r.entities.len(),
            1,
            "the authoritative field-index backfill makes the non-keyed Path lookup resolve /souls"
        );
        assert_eq!(r.entities[0]["entity_id"], "fx-souls");
        assert_eq!(r.telemetry.coverage.missing, 0);
        assert_eq!(r.telemetry.materialized_count, 1);
        assert_eq!(
            r.telemetry.strategy,
            QueryPlaneReadStrategy::NativePagePushdown
        );
        let (ids, pending) = store
            .reconciled_equality_candidates_pg(
                tenant.as_str(),
                "Directory",
                &[
                    ("Path".into(), "/souls".into()),
                    ("WorkspaceId".into(), "wsA".into()),
                ],
                None,
            )
            .await
            .unwrap();
        assert_eq!(ids, vec!["fx-souls"]);
        assert!(pending.is_empty());
    });
}
