//! ARN-68 / ADR-0153 — the declared-key existence oracle: registry-aware key
//! resolution (A), authoritative backfill (B), and watermark-gated authoritative
//! absence (C). Exercised against the sim store, the DST-canonical backend that
//! co-commits key rows and maintains the watermark exactly like prod Postgres — and,
//! for the full souls scenario, against **real Postgres** (the prod engine) via
//! `directory_root_souls_scenario_on_postgres` (gated on `DATABASE_URL`).

use super::*;
use temper_runtime::persistence::EventStore;
use temper_store_sim::{SimEventStore, SimFaultConfig};

#[test]
fn declared_keys_resolve_from_registry_not_just_transition_tables() {
    // ARN-68 root cause: runtime-installed os-app entities (File, Directory, …)
    // are registered in the per-tenant SpecRegistry, NOT in `state.transition_tables`
    // (which is only ever set by `with_specs` at boot). The keyed read fast path
    // must resolve declared keys from the registry — reading `transition_tables`
    // alone returns nothing, silently disabling the keyed path so every point read
    // scans and 413s at scale.
    let state = build_order_state("declared-keys-registry");
    let tenant = TenantId::default();

    // Precondition that reproduces the bug: Order is registry-only.
    assert!(
        state.transition_tables.get("Order").is_none(),
        "Order is registered in the registry, not transition_tables (the openpaw case)"
    );

    // The fix: declared_keys_for resolves it via the registry.
    let keys = state.declared_keys_for(&tenant, "Order");
    assert_eq!(
        keys.len(),
        1,
        "the declared [[key]] must be found via the registry"
    );
    assert_eq!(keys[0].name, "ws_path");
    assert_eq!(
        keys[0].properties,
        vec!["WorkspaceId".to_string(), "Path".to_string()]
    );

    // And an unregistered type still yields no keys (no false positives).
    assert!(state.declared_keys_for(&tenant, "Nonexistent").is_empty());
}

/// Build an Order state backed by the in-memory sim store — the DST-canonical store
/// that co-commits key rows AND maintains the backfill watermark (ADR-0153), so it is
/// a *sound* keyed backend (unlike Turso, which does not co-commit keys). Returns the
/// store handle so a test can assert on `entity_key_index`/watermark state directly.
fn build_order_state_with_sim(system_name: &str) -> (ServerState, SimEventStore) {
    let store = SimEventStore::new(0, SimFaultConfig::none());
    let mut state = build_order_state(system_name);
    state.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    (state, store)
}

const DIRECTORY_IOA: &str =
    include_str!("../../../../../../test-fixtures/specs/directory.ioa.toml");

/// Sim-backed state with the REAL paw-fs Directory spec registered — the 3-part
/// `name_parent` key `[Name, WorkspaceId, ParentId]`, where roots have NO `ParentId`
/// field at all. This is the exact shape behind the soul-write 413; the earlier
/// keyed tests used a 2-part all-present key and never exercised a 3-part key with an
/// absent component through the backfill (the gap that misled the prod read).
fn build_dir_state_with_sim(system_name: &str) -> (ServerState, SimEventStore) {
    let store = SimEventStore::new(0, SimFaultConfig::none());
    let csdl = parse_csdl(CSDL_XML).expect("CSDL parses");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        TenantId::default().as_str(),
        csdl,
        CSDL_XML.to_string(),
        &[("Order", ORDER_IOA), ("Directory", DIRECTORY_IOA)],
    );
    let mut state = ServerState::from_registry(ActorSystem::new(system_name), registry);
    state.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    (state, store)
}

/// The real `ensure_dirs` root lookup: `Name eq '/' and WorkspaceId eq <ws> and ParentId eq null`.
fn dir_root_filter(ws: &str) -> FilterExpr {
    let eq_s = |p: &str, v: &str| FilterExpr::BinaryOp {
        left: Box::new(FilterExpr::Property(p.to_string())),
        op: BinaryOperator::Eq,
        right: Box::new(FilterExpr::Literal(ODataValue::String(v.to_string()))),
    };
    let parentid_eq_null = FilterExpr::BinaryOp {
        left: Box::new(FilterExpr::Property("ParentId".to_string())),
        op: BinaryOperator::Eq,
        right: Box::new(FilterExpr::Literal(ODataValue::Null)),
    };
    FilterExpr::BinaryOp {
        left: Box::new(FilterExpr::BinaryOp {
            left: Box::new(eq_s("Name", "/")),
            op: BinaryOperator::And,
            right: Box::new(eq_s("WorkspaceId", ws)),
        }),
        op: BinaryOperator::And,
        right: Box::new(parentid_eq_null),
    }
}

/// `canonical_key_hash` of a Directory root (`Name`,`WorkspaceId` present, `ParentId` absent).
fn dir_root_hash(ws: &str) -> String {
    let mut fields = serde_json::Map::new();
    fields.insert("Name".to_string(), serde_json::json!("/"));
    fields.insert("WorkspaceId".to_string(), serde_json::json!(ws));
    // ParentId absent on purpose — the root case.
    crate::key_index::canonical_key_hash(
        "name_parent",
        &[
            "Name".to_string(),
            "WorkspaceId".to_string(),
            "ParentId".to_string(),
        ],
        &fields,
    )
    .expect("a root (Name+WorkspaceId present, ParentId absent) must be keyable")
}

/// Create a Directory via a Create event that carries the real PascalCase fields paw-fs
/// stores (`Name`/`Path`/`WorkspaceId`, and `ParentId` only for non-roots), so the
/// entity is enumerable AND its fields are present on both replay and the live actor.
async fn seed_dir(
    state: &ServerState,
    agent_ctx: &AgentContext,
    eid: &str,
    name: &str,
    path: &str,
    ws: &str,
    parent: Option<&str>,
) {
    let tenant = TenantId::default();
    // Create persists the supplied payload map verbatim into the entity's fields (the
    // IOA's snake_case `params` list names the action's logical inputs but does not
    // rename or gate them), so we send the PascalCase field names paw-fs stores in prod
    // (`Name`/`Path`/`WorkspaceId`/`ParentId`). That way the fields are populated on
    // replay AND in the live actor — the backfill (recover) and the read materialization
    // (get_tenant_entity_state) see identical fields — and a root simply omits `ParentId`.
    let mut params = serde_json::Map::new();
    params.insert("Name".to_string(), serde_json::json!(name));
    params.insert("Path".to_string(), serde_json::json!(path));
    params.insert("WorkspaceId".to_string(), serde_json::json!(ws));
    if let Some(p) = parent {
        params.insert("ParentId".to_string(), serde_json::json!(p));
    }
    state
        .dispatch_tenant_action(
            &tenant,
            "Directory",
            eid,
            "Create",
            serde_json::Value::Object(params),
            agent_ctx,
        )
        .await
        .expect("create directory");
}

/// THE souls-scenario proof, with the REAL Directory spec/key and the duplicate-root
/// case that misled the prod read. End-to-end through the real read path:
///   1. > budget directories incl. a root (absent ParentId) + DUPLICATE roots (same
///      key) + subdirs → a non-keyed root lookup scans > budget and 413s (the bug).
///   2. The backfill keys the root (absent ParentId → null), correctly skips the
///      duplicate roots as key-conflicts (not failures), and watermarks Directory.
///   3. Post-backfill: a NEW workspace's root lookup resolves to authoritative-absent
///      (empty, NO 413) — so `ensure_dirs` creates the root and the soul write
///      proceeds; and the existing workspace's root lookup resolves (keyed hit).
#[tokio::test]
async fn directory_root_lookup_souls_scenario_with_real_key_and_duplicates() {
    let (state, store) = build_dir_state_with_sim("dir-souls");
    let tenant = TenantId::default();
    let agent_ctx = AgentContext::for_service("dir-souls-test");
    let security_ctx = SecurityContext::system();

    // Workspace wsA: a real root (no ParentId), TWO duplicate roots (same key), and
    // enough subdirs that the total Directory count exceeds the scan budget below.
    seed_dir(&state, &agent_ctx, "wsA-root", "/", "/", "wsA", None).await;
    seed_dir(&state, &agent_ctx, "wsA-root-dup1", "/", "/", "wsA", None).await;
    seed_dir(&state, &agent_ctx, "wsA-root-dup2", "/", "/", "wsA", None).await;
    for i in 0..12 {
        seed_dir(
            &state,
            &agent_ctx,
            &format!("wsA-sub-{i}"),
            &format!("sub{i}"),
            &format!("/sub{i}"),
            "wsA",
            Some("wsA-root"),
        )
        .await;
    }

    // Budget so the full-type scan (15 dirs) exceeds it (max_entities=1 → budget 10).
    let budget = QueryPlaneReadBudget {
        default_page_size: 1,
        max_entities: 1,
    };
    let qo_b = QueryOptions {
        filter: Some(dir_root_filter("wsB")),
        ..QueryOptions::default()
    };
    let qo_a = QueryOptions {
        filter: Some(dir_root_filter("wsA")),
        ..QueryOptions::default()
    };

    // (1) Before the watermark: a NEW workspace's root lookup misses → scans 15 > budget → 413.
    match read_entity_set_page(QueryPlaneReadRequest {
        state: &state,
        tenant: &tenant,
        security_ctx: &security_ctx,
        entity_type: "Directory",
        entity_set_name: "Directories",
        query_options: &qo_b,
        budget,
    })
    .await
    {
        Err(QueryPlaneReadError::QueryTooLarge { .. }) => {}
        Ok(_) => panic!("expected 413 for a missing root before watermark, got Ok"),
        Err(_) => panic!("expected QueryTooLarge before watermark, got another error"),
    }

    // (2) Backfill: clear the lazy index (fresh-boot), run it, confirm the real root
    // is keyed, the duplicates collided (still watermarked), and the type is watermarked.
    state.entity_index.write().unwrap().clear();
    state.entity_index_hydrated.write().unwrap().clear();
    state.populate_key_index_from_snapshots(&tenant).await;
    assert!(
        state
            .key_index_backfill_complete(&tenant, "Directory", "name_parent")
            .await,
        "Directory must watermark — duplicate roots are key-conflict skips, not failures"
    );
    assert!(
        store
            .lookup_by_key(
                tenant.as_str(),
                "Directory",
                "name_parent",
                &dir_root_hash("wsA")
            )
            .await
            .unwrap()
            .is_some(),
        "the wsA root (absent ParentId) must be keyed"
    );

    // (3a) The souls case: a NEW workspace (no root) → authoritative-absent → empty,
    // NO 413. This is what lets ensure_dirs create the root and the soul write proceed.
    let r = match read_entity_set_page(QueryPlaneReadRequest {
        state: &state,
        tenant: &tenant,
        security_ctx: &security_ctx,
        entity_type: "Directory",
        entity_set_name: "Directories",
        query_options: &qo_b,
        budget,
    })
    .await
    {
        Ok(r) => r,
        Err(_) => panic!("no 413 once watermarked — keyed miss must be authoritative absence"),
    };
    assert!(
        r.entities.is_empty(),
        "a workspace with no root resolves to absent"
    );
    assert_eq!(
        r.telemetry.fallback_reason,
        QueryPlaneFallbackReason::KeyedAbsence
    );

    // (3b) An existing workspace's root lookup resolves (hits the one keyed root,
    // despite the duplicates) — no 413, returns exactly one root.
    let r = match read_entity_set_page(QueryPlaneReadRequest {
        state: &state,
        tenant: &tenant,
        security_ctx: &security_ctx,
        entity_type: "Directory",
        entity_set_name: "Directories",
        query_options: &qo_a,
        budget,
    })
    .await
    {
        Ok(r) => r,
        Err(_) => panic!("existing root must resolve without 413"),
    };
    assert_eq!(
        r.entities.len(),
        1,
        "wsA root lookup resolves to the keyed root"
    );
}

/// Seed a Directory on a Postgres-backed state under an explicit tenant (the PG souls
/// test uses a unique tenant for isolation). `spec` = `(name, path, ws, parent)`.
async fn pg_seed_dir(
    state: &ServerState,
    tenant: &TenantId,
    agent_ctx: &AgentContext,
    eid: &str,
    spec: (&str, &str, &str, Option<&str>),
) {
    let (name, path, ws, parent) = spec;
    let mut params = serde_json::Map::new();
    params.insert("Name".to_string(), serde_json::json!(name));
    params.insert("Path".to_string(), serde_json::json!(path));
    params.insert("WorkspaceId".to_string(), serde_json::json!(ws));
    if let Some(p) = parent {
        params.insert("ParentId".to_string(), serde_json::json!(p));
    }
    state
        .dispatch_tenant_action(
            tenant,
            "Directory",
            eid,
            "Create",
            serde_json::Value::Object(params),
            agent_ctx,
        )
        .await
        .expect("create directory");
}

/// THE souls scenario on **real Postgres** (the prod engine — `from_postgres` wires the
/// event store AND the query-plane), gated on `DATABASE_URL`; unique tenant for
/// isolation. Same shape as the sim proof, but against the actual backend so the keyed
/// index, the `key_index_backfill_watermark` table, and the materialization run as they
/// do in production: the 413 reproduces, the backfill watermarks despite duplicate
/// roots, a NEW workspace's root lookup is authoritative-absent (empty, NO 413 — the
/// soul write's exact path), and the existing absent-`ParentId` root resolves.
#[test]
fn directory_root_souls_scenario_on_postgres() {
    use temper_runtime::scheduler::sim_uuid as runtime_sim_uuid;
    let database_url = match std::env::var("DATABASE_URL") {
        Ok(url) => url,
        Err(_) => return,
    };
    sqlx::test_block_on(async {
        let pool = sqlx::PgPool::connect(&database_url).await.unwrap();
        temper_store_postgres::migration::run_migrations(&pool)
            .await
            .unwrap();
        let store = temper_store_postgres::PostgresEventStore::new(pool.clone());

        let tenant = TenantId::from(format!("souls-pg-{}", runtime_sim_uuid()));
        let csdl = parse_csdl(CSDL_XML).expect("CSDL parses");
        let mut registry = SpecRegistry::new();
        registry.register_tenant(
            tenant.as_str(),
            csdl,
            CSDL_XML.to_string(),
            &[("Order", ORDER_IOA), ("Directory", DIRECTORY_IOA)],
        );
        let mut state = ServerState::from_registry(ActorSystem::new("dir-souls-pg"), registry);
        state.set_storage_stack(StorageStack::from_postgres(store.clone()));
        let agent_ctx = AgentContext::for_service("dir-souls-pg-test");
        let security_ctx = SecurityContext::system();

        // wsA: a real root (no ParentId), two duplicate roots (same key), 12 subdirs.
        pg_seed_dir(
            &state,
            &tenant,
            &agent_ctx,
            "wsA-root",
            ("/", "/", "wsA", None),
        )
        .await;
        pg_seed_dir(
            &state,
            &tenant,
            &agent_ctx,
            "wsA-root-d1",
            ("/", "/", "wsA", None),
        )
        .await;
        pg_seed_dir(
            &state,
            &tenant,
            &agent_ctx,
            "wsA-root-d2",
            ("/", "/", "wsA", None),
        )
        .await;
        for i in 0..12 {
            pg_seed_dir(
                &state,
                &tenant,
                &agent_ctx,
                &format!("wsA-sub-{i}"),
                (
                    &format!("sub{i}"),
                    &format!("/sub{i}"),
                    "wsA",
                    Some("wsA-root"),
                ),
            )
            .await;
        }

        let budget = QueryPlaneReadBudget {
            default_page_size: 1,
            max_entities: 1,
        };
        let qo_b = QueryOptions {
            filter: Some(dir_root_filter("wsB")),
            ..QueryOptions::default()
        };
        let qo_a = QueryOptions {
            filter: Some(dir_root_filter("wsA")),
            ..QueryOptions::default()
        };
        // (1) Pre-watermark: a NEW workspace's root lookup misses. Historically this
        // scanned 15 > budget → 413 (the original prod failure). The ARN-68 empty-list
        // gap reconcile now bounds it: roots have NO ParentId field-index row, so they
        // form the (small) coverage gap, get materialized, and `ParentId eq null`
        // matches none for wsB → a bounded EMPTY answer instead of the 413. Were the
        // gap larger than the budget (prod's 1688 duplicate roots), the 413 would
        // remain — the keyed path below stays the authoritative fix for roots.
        match read_entity_set_page(QueryPlaneReadRequest {
            state: &state,
            tenant: &tenant,
            security_ctx: &security_ctx,
            entity_type: "Directory",
            entity_set_name: "Directories",
            query_options: &qo_b,
            budget,
        })
        .await
        {
            Ok(result) => assert!(
                result.entities.is_empty(),
                "wsB has no root; the bounded gap reconcile must return empty"
            ),
            Err(_) => panic!("the gap reconcile must bound the pre-watermark root miss"),
        }

        // (2) Backfill on real Postgres → Directory watermarked (duplicate roots are
        // key-conflict skips, not failures), and the absent-ParentId root is keyed.
        state.populate_key_index_from_snapshots(&tenant).await;
        assert!(
            state
                .key_index_backfill_complete(&tenant, "Directory", "name_parent")
                .await,
            "Directory must watermark on Postgres despite duplicate roots"
        );
        assert!(
            store
                .lookup_by_key(
                    tenant.as_str(),
                    "Directory",
                    "name_parent",
                    &dir_root_hash("wsA")
                )
                .await
                .unwrap()
                .is_some(),
            "the wsA root (absent ParentId) is keyed on Postgres"
        );

        // (3a) The soul-write path: a NEW workspace (no root) → authoritative-absent →
        // empty, NO 413 → ensure_dirs creates the root and the soul write proceeds.
        let r = match read_entity_set_page(QueryPlaneReadRequest {
            state: &state,
            tenant: &tenant,
            security_ctx: &security_ctx,
            entity_type: "Directory",
            entity_set_name: "Directories",
            query_options: &qo_b,
            budget,
        })
        .await
        {
            Ok(r) => r,
            Err(_) => panic!("no 413 once watermarked — keyed miss is authoritative absence"),
        };
        assert!(
            r.entities.is_empty(),
            "a workspace with no root resolves to absent"
        );
        assert_eq!(
            r.telemetry.fallback_reason,
            QueryPlaneFallbackReason::KeyedAbsence
        );

        // (3b) The existing absent-ParentId root resolves (no 413, exactly one).
        let r = match read_entity_set_page(QueryPlaneReadRequest {
            state: &state,
            tenant: &tenant,
            security_ctx: &security_ctx,
            entity_type: "Directory",
            entity_set_name: "Directories",
            query_options: &qo_a,
            budget,
        })
        .await
        {
            Ok(r) => r,
            Err(_) => panic!("existing root must resolve without 413"),
        };
        assert_eq!(
            r.entities.len(),
            1,
            "wsA root resolves to the keyed root on Postgres"
        );
    });
}

/// ARN-68 (generic bound): the FIELD-index backfill must enumerate authoritatively
/// (registry + `store.list_entity_ids_by_type`), not the lazy `entity_index`. The flow
/// looks up directories by `Path` (e.g. `Path eq '/souls' and WorkspaceId eq …`), which
/// is NOT a declared key — so it relies on the field index. If the backfill reads the
/// lazy index (near-empty at boot), pre-existing dirs stay unindexed and that lookup
/// falls back to the full scan → 413 at scale. Proven on real Postgres: with the field
/// index empty the Path lookup 413s, and after the (authoritative) backfill it binds via
/// the native page. Gated on DATABASE_URL; unique tenant.
#[test]
fn field_index_backfill_bounds_non_keyed_path_lookup_on_postgres() {
    use temper_runtime::scheduler::sim_uuid as runtime_sim_uuid;
    let database_url = match std::env::var("DATABASE_URL") {
        Ok(url) => url,
        Err(_) => return,
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

        // Fresh-boot, pre-existing-data state: the lazy index is empty (what the pre-fix
        // backfill read), and the field index has no rows for these dirs (as it would for
        // entities written before the projection existed). Clearing both is the exact
        // "old dirs the backfill must reach" condition.
        state.entity_index.write().unwrap().clear();
        state.entity_index_hydrated.write().unwrap().clear();
        sqlx::query("DELETE FROM entity_field_index WHERE tenant = $1")
            .bind(tenant.as_str())
            .execute(&pool)
            .await
            .unwrap();

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

        // (1) Field index empty (pre-existing dirs unindexed): the non-keyed Path lookup
        // enumerates the full type from the store (> budget) with nothing to narrow it →
        // 413 QueryTooLarge — exactly the prod failure.
        match read_entity_set_page(QueryPlaneReadRequest {
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
            Err(QueryPlaneReadError::QueryTooLarge { .. }) => {}
            Ok(_) => panic!("expected 413 for the Path lookup before the field-index backfill"),
            Err(_) => panic!("expected QueryTooLarge before the field-index backfill"),
        }

        // (2) The FIXED field-index backfill enumerates authoritatively (registry +
        // store), NOT the cleared lazy index, and indexes the pre-existing dirs. The OLD
        // lazy-index enumeration would index nothing here and /souls would stay invisible.
        // The enumeration has NO declared-key branch (unlike the key-index backfill), so
        // it covers every registered type identically — keyed or not; this case exercises
        // a non-key FIELD (`Path`) on a keyed type, which is the field index's job.
        state.populate_field_index_from_snapshots(&tenant).await;

        // (3) The same Path lookup now binds via the native page and finds /souls — proof
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
    });
}

/// `WorkspaceId eq <ws> and Path eq <path>` — the shape that resolves to Order's
/// declared `ws_path` key.
fn ws_path_filter(ws: &str, path: &str) -> FilterExpr {
    let eq = |prop: &str, val: &str| FilterExpr::BinaryOp {
        left: Box::new(FilterExpr::Property(prop.to_string())),
        op: BinaryOperator::Eq,
        right: Box::new(FilterExpr::Literal(ODataValue::String(val.to_string()))),
    };
    FilterExpr::BinaryOp {
        left: Box::new(eq("WorkspaceId", ws)),
        op: BinaryOperator::And,
        right: Box::new(eq("Path", path)),
    }
}

fn ws_path_hash(ws: &str, path: &str) -> String {
    let mut fields = serde_json::Map::new();
    fields.insert("WorkspaceId".to_string(), serde_json::json!(ws));
    fields.insert("Path".to_string(), serde_json::json!(path));
    crate::key_index::canonical_key_hash(
        "ws_path",
        &["WorkspaceId".to_string(), "Path".to_string()],
        &fields,
    )
    .expect("both key components present")
}

/// B (ADR-0153): the backfill must key EXISTING entities by enumerating the durable
/// store, not the lazy in-memory `entity_index`. We create keyed orders, then clear
/// the in-memory index to simulate a fresh boot (the OLD backfill enumerated that and
/// would key nothing), and prove the backfill still keys every entity and watermarks
/// the type.
#[tokio::test]
async fn key_index_backfill_keys_store_entities_absent_from_the_lazy_index() {
    let (state, store) = build_order_state_with_sim("key-backfill");
    let tenant = TenantId::default();
    let agent_ctx = AgentContext::for_service("key-backfill-test");
    let entities = [("ord-key-0", "ws1", "/a"), ("ord-key-1", "ws1", "/b")];
    for (eid, ws, path) in entities {
        // A Create event makes the entity enumerable by the durable store scan…
        state
            .dispatch_tenant_action(
                &tenant,
                "Order",
                eid,
                "Create",
                serde_json::json!({}),
                &agent_ctx,
            )
            .await
            .expect("create order");
        // …and a snapshot carries its key-valued fields (an entity that existed with
        // these fields before the [[key]] was declared — the backfill's target).
        let snapshot = serde_json::json!({
            "entity_type": "Order",
            "entity_id": eid,
            "status": "Draft",
            "item_count": 0,
            "fields": { "Id": eid, "Status": "Draft", "WorkspaceId": ws, "Path": path },
        });
        store
            .save_snapshot(
                &format!("{tenant}:Order:{eid}"),
                1,
                &serde_json::to_vec(&snapshot).unwrap(),
            )
            .await
            .expect("seed snapshot");
    }

    // Nothing is keyed yet: the entities were seeded via `save_snapshot` with no
    // key-bearing Create event, so the sim store's live co-commit saw no key fields.
    // The keyed fields exist only in the snapshot — exactly the "pre-existing entity
    // the backfill must key" case.
    assert!(
        store
            .lookup_by_key(
                tenant.as_str(),
                "Order",
                "ws_path",
                &ws_path_hash("ws1", "/a")
            )
            .await
            .unwrap()
            .is_none(),
        "precondition: not keyed before backfill"
    );

    // Fresh-boot simulation: the lazy index is empty. The pre-fix backfill read this.
    state.entity_index.write().unwrap().clear();
    state.entity_index_hydrated.write().unwrap().clear();
    assert!(state.list_entity_ids(&tenant, "Order").is_empty());

    state.populate_key_index_from_snapshots(&tenant).await;

    // Enumerated from the store and keyed both entities, and watermarked the type.
    assert!(
        state
            .key_index_backfill_complete(&tenant, "Order", "ws_path")
            .await,
        "Order must be watermarked after a clean backfill"
    );
    for (ws, path) in [("ws1", "/a"), ("ws1", "/b")] {
        assert!(
            store
                .lookup_by_key(tenant.as_str(), "Order", "ws_path", &ws_path_hash(ws, path))
                .await
                .unwrap()
                .is_some(),
            "backfill must key {ws}{path}"
        );
    }
}

/// Robustness (ADR-0153): the backfill is RESUMABLE — already-keyed entities are
/// skipped (not re-loaded), so a re-run after a partial pass only processes the
/// remainder instead of re-loading all N. Pre-key one entity directly, then run the
/// backfill, and confirm it completes + watermarks with both entities keyed.
#[tokio::test]
async fn key_index_backfill_skips_already_keyed_entities_and_still_watermarks() {
    let (state, store) = build_order_state_with_sim("key-backfill-resume");
    let tenant = TenantId::default();
    let agent_ctx = AgentContext::for_service("resume-test");
    for (eid, ws, path) in [("ord-a", "ws1", "/a"), ("ord-b", "ws1", "/b")] {
        state
            .dispatch_tenant_action(
                &tenant,
                "Order",
                eid,
                "Create",
                serde_json::json!({}),
                &agent_ctx,
            )
            .await
            .expect("create");
        let snap = serde_json::json!({
            "entity_type": "Order", "entity_id": eid, "status": "Draft", "item_count": 0,
            "fields": { "Id": eid, "WorkspaceId": ws, "Path": path },
        });
        store
            .save_snapshot(
                &format!("{tenant}:Order:{eid}"),
                1,
                &serde_json::to_vec(&snap).unwrap(),
            )
            .await
            .expect("snap");
    }
    // Pre-key ord-a directly (a prior partial pass / co-commit already keyed it).
    store
        .backfill_entity_keys(
            tenant.as_str(),
            "Order",
            "ord-a",
            &[temper_runtime::persistence::EntityKeyRow {
                key_name: "ws_path".to_string(),
                key_hash: ws_path_hash("ws1", "/a"),
            }],
        )
        .await
        .expect("pre-key");

    state.populate_key_index_from_snapshots(&tenant).await;

    // ord-a was skipped via the already-keyed set; ord-b keyed fresh; type watermarked.
    assert!(
        state
            .key_index_backfill_complete(&tenant, "Order", "ws_path")
            .await
    );
    for (ws, path) in [("ws1", "/a"), ("ws1", "/b")] {
        assert!(
            store
                .lookup_by_key(tenant.as_str(), "Order", "ws_path", &ws_path_hash(ws, path))
                .await
                .unwrap()
                .is_some()
        );
    }
}

/// Soundness (ADR-0153): a DELETED entity is correctly skipped (not keyed) and does
/// NOT block the watermark — only entities that exist-but-cannot-load do. A deleted
/// entity alongside a live one: the type still watermarks, the live one is keyed, the
/// deleted one is not.
#[tokio::test]
async fn key_index_backfill_skips_deleted_entities_without_blocking_watermark() {
    let (state, store) = build_order_state_with_sim("key-backfill-deleted");
    let tenant = TenantId::default();
    let agent_ctx = AgentContext::for_service("deleted-test");
    for (eid, status, path) in [
        ("ord-live", "Draft", "/live"),
        ("ord-del", "Deleted", "/del"),
    ] {
        state
            .dispatch_tenant_action(
                &tenant,
                "Order",
                eid,
                "Create",
                serde_json::json!({}),
                &agent_ctx,
            )
            .await
            .expect("create");
        let snap = serde_json::json!({
            "entity_type": "Order", "entity_id": eid, "status": status, "item_count": 0,
            "fields": { "Id": eid, "WorkspaceId": "ws1", "Path": path },
        });
        store
            .save_snapshot(
                &format!("{tenant}:Order:{eid}"),
                1,
                &serde_json::to_vec(&snap).unwrap(),
            )
            .await
            .expect("snap");
    }

    state.populate_key_index_from_snapshots(&tenant).await;

    assert!(
        state
            .key_index_backfill_complete(&tenant, "Order", "ws_path")
            .await,
        "a deleted entity must not block the watermark"
    );
    assert!(
        store
            .lookup_by_key(
                tenant.as_str(),
                "Order",
                "ws_path",
                &ws_path_hash("ws1", "/live")
            )
            .await
            .unwrap()
            .is_some(),
        "live entity is keyed"
    );
    assert!(
        store
            .lookup_by_key(
                tenant.as_str(),
                "Order",
                "ws_path",
                &ws_path_hash("ws1", "/del")
            )
            .await
            .unwrap()
            .is_none(),
        "deleted entity is not keyed"
    );
}

/// Soundness gate (ADR-0153): an entity that EXISTS but whose journal cannot be read
/// is classified `LoadFailed` — it must NOT be keyed AND must block the watermark
/// (otherwise a keyed miss for it would wrongly read as authoritative absence). The
/// backfill then resumes on a later run once the read succeeds. Without this, a
/// transient journal-read error during backfill would silently produce a permanent
/// wrong-absent.
#[tokio::test]
async fn key_index_backfill_loadfailed_entity_blocks_watermark_then_resumes() {
    let (state, store) = build_order_state_with_sim("key-backfill-loadfail");
    let tenant = TenantId::default();
    let agent_ctx = AgentContext::for_service("loadfail-test");
    let pid = format!("{tenant}:Order:ord-x");
    state
        .dispatch_tenant_action(
            &tenant,
            "Order",
            "ord-x",
            "Create",
            serde_json::json!({}),
            &agent_ctx,
        )
        .await
        .expect("create");
    let snap = serde_json::json!({
        "entity_type": "Order", "entity_id": "ord-x", "status": "Draft", "item_count": 0,
        "total_event_count": 1,
        "fields": { "Id": "ord-x", "WorkspaceId": "ws1", "Path": "/x" },
    });
    store
        .save_snapshot(&pid, 1, &serde_json::to_vec(&snap).unwrap())
        .await
        .expect("snap");

    // Run 1: the entity's journal read fails → LoadFailed → type NOT watermarked,
    // entity NOT keyed.
    store.fail_next_reads(&pid, 1);
    state.populate_key_index_from_snapshots(&tenant).await;
    assert!(
        !state
            .key_index_backfill_complete(&tenant, "Order", "ws_path")
            .await,
        "an unloadable entity must block the watermark"
    );
    assert!(
        store
            .lookup_by_key(
                tenant.as_str(),
                "Order",
                "ws_path",
                &ws_path_hash("ws1", "/x")
            )
            .await
            .unwrap()
            .is_none(),
        "the unloadable entity must not be keyed"
    );

    // Run 2 (resume): the read now succeeds → entity keyed → type watermarked.
    state.populate_key_index_from_snapshots(&tenant).await;
    assert!(
        state
            .key_index_backfill_complete(&tenant, "Order", "ws_path")
            .await,
        "backfill must resume and watermark once the read succeeds"
    );
    assert!(
        store
            .lookup_by_key(
                tenant.as_str(),
                "Order",
                "ws_path",
                &ws_path_hash("ws1", "/x")
            )
            .await
            .unwrap()
            .is_some(),
        "the entity is keyed on resume"
    );
}

/// C (ADR-0153): once the backfill watermark is set, a keyed read MISS is
/// authoritative absence — the read returns empty WITHOUT the full-type scan that
/// otherwise 413s at scale (ARN-68). Before the watermark, the same miss falls back
/// to the scan and 413s. This is the end-to-end proof that the fix removes the 413.
#[tokio::test]
async fn keyed_miss_returns_empty_without_scan_413_once_watermarked() {
    let (state, _store) = build_order_state_with_sim("keyed-absence");
    let tenant = TenantId::default();
    let security_ctx = SecurityContext::system();
    // More orders than the scan budget (max_entities=1 → scan_candidate_budget=10),
    // so the fallback scan would trip the budget.
    create_orders(&state, 11).await;

    let query_options = QueryOptions {
        // Resolves to ws_path but matches no entity (a genuine miss).
        filter: Some(ws_path_filter("nope", "/none")),
        ..QueryOptions::default()
    };
    let budget = QueryPlaneReadBudget {
        default_page_size: 1,
        max_entities: 1,
    };

    // Before the watermark: keyed miss → scan fallback → 413.
    match read_entity_set_page(QueryPlaneReadRequest {
        state: &state,
        tenant: &tenant,
        security_ctx: &security_ctx,
        entity_type: "Order",
        entity_set_name: "Orders",
        query_options: &query_options,
        budget,
    })
    .await
    {
        Err(QueryPlaneReadError::QueryTooLarge { .. }) => {}
        Ok(_) => panic!("expected 413 before watermark, got Ok"),
        Err(_) => panic!("expected QueryTooLarge before watermark, got another error"),
    }

    // Watermark Order → a keyed miss is now authoritative absence.
    state
        .mark_key_index_backfilled(&tenant, "Order", "ws_path")
        .await;

    let result = match read_entity_set_page(QueryPlaneReadRequest {
        state: &state,
        tenant: &tenant,
        security_ctx: &security_ctx,
        entity_type: "Order",
        entity_set_name: "Orders",
        query_options: &query_options,
        budget,
    })
    .await
    {
        Ok(result) => result,
        Err(_) => panic!("no 413 once watermarked — keyed miss must be authoritative absence"),
    };
    assert!(result.entities.is_empty(), "a genuine miss returns no rows");
    assert_eq!(
        result.telemetry.fallback_reason,
        QueryPlaneFallbackReason::KeyedAbsence,
        "the read must resolve via keyed absence, not a scan"
    );
}

/// ARN-68: declaring an ADDITIONAL key on a type that was already backfilled must re-key
/// the existing entities. The watermark is key-set aware, so a watermark that covered an
/// EARLIER declaration is NOT treated as complete for the newly-declared key — the
/// backfill force-re-keys every existing entity, and until it does the read must NOT
/// claim authoritative absence for the new key (that would read a present entity as "not
/// found", a silent wrong answer). Simulates the prod case behind ARN-68's second 413:
/// directories keyed under `name_parent`, then `ws_path` added — next boot re-keys.
#[tokio::test]
async fn key_index_backfill_rekeys_existing_entities_when_a_key_is_added() {
    let (state, store) = build_order_state_with_sim("key-rekey");
    let tenant = TenantId::default();
    let agent_ctx = AgentContext::for_service("key-rekey-test");

    // Two orders that ALREADY have a key row under an OLDER key name, and whose
    // ws_path-valued fields live in the snapshot — the exact prod shape: entities keyed
    // under an earlier declaration (here `old_key`), the new key (`ws_path`) not yet
    // assigned. The old-key rows put them in `keyed_entity_ids_for_type`, so the
    // per-entity resumability skip WOULD skip them — this is what `force_full_rekey`
    // must bypass. Without the bypass this test fails (ws_path never gets assigned).
    for (eid, ws, path) in [("ord-rk-0", "ws1", "/a"), ("ord-rk-1", "ws1", "/b")] {
        state
            .dispatch_tenant_action(
                &tenant,
                "Order",
                eid,
                "Create",
                serde_json::json!({}),
                &agent_ctx,
            )
            .await
            .expect("create order");
        let snapshot = serde_json::json!({
            "entity_type": "Order", "entity_id": eid, "status": "Draft", "item_count": 0,
            "fields": { "Id": eid, "Status": "Draft", "WorkspaceId": ws, "Path": path },
        });
        store
            .save_snapshot(
                &format!("{tenant}:Order:{eid}"),
                1,
                &serde_json::to_vec(&snapshot).unwrap(),
            )
            .await
            .expect("seed snapshot");
        // Key it under the OLD key only (so it appears already-keyed for resumability).
        store
            .backfill_entity_keys(
                tenant.as_str(),
                "Order",
                eid,
                &[temper_runtime::persistence::EntityKeyRow {
                    key_name: "old_key".to_string(),
                    key_hash: format!("old-hash-{eid}"),
                }],
            )
            .await
            .expect("pre-key under the old key");
    }
    state.entity_index.write().unwrap().clear();
    state.entity_index_hydrated.write().unwrap().clear();

    // Watermarked under the earlier declaration (`old_key`), which did NOT cover ws_path.
    state
        .mark_key_index_backfilled(&tenant, "Order", "old_key")
        .await;

    // Read gate: covered ("old_key") != current ("ws_path"), so the type reads as
    // INCOMPLETE for ws_path — a ws_path miss falls back to the scan, never a wrong absent.
    assert!(
        !state
            .key_index_backfill_complete(&tenant, "Order", "ws_path")
            .await,
        "a stale watermark covering a different key-set must read as incomplete for the new key"
    );
    // The entities ARE already-keyed (under old_key) — so the resumability skip would
    // exclude them; only force_full_rekey re-processes them.
    assert!(
        !store
            .keyed_entity_ids_for_type(tenant.as_str(), "Order")
            .await
            .unwrap()
            .is_empty(),
        "precondition: entities appear already-keyed (old_key), so the resume-skip would skip them"
    );
    assert!(
        store
            .lookup_by_key(
                tenant.as_str(),
                "Order",
                "ws_path",
                &ws_path_hash("ws1", "/a")
            )
            .await
            .unwrap()
            .is_none(),
        "precondition: not yet keyed for the newly-added ws_path key"
    );

    // Re-run the backfill: covered != current declared → force-full re-key of every
    // existing entity, then re-watermark with the current key-set.
    state.populate_key_index_from_snapshots(&tenant).await;

    assert!(
        state
            .key_index_backfill_complete(&tenant, "Order", "ws_path")
            .await,
        "after the re-key the type is complete for the current declared key-set"
    );
    for (eid, ws, path) in [("ord-rk-0", "ws1", "/a"), ("ord-rk-1", "ws1", "/b")] {
        assert_eq!(
            store
                .lookup_by_key(tenant.as_str(), "Order", "ws_path", &ws_path_hash(ws, path))
                .await
                .unwrap()
                .as_deref(),
            Some(eid),
            "the added key resolves the pre-existing entity after the re-key"
        );
    }
}

#[test]
fn declared_key_set_signature_is_sorted_and_joined() {
    use temper_jit::table::types::DeclaredKey;
    let key = |name: &str| DeclaredKey {
        name: name.to_string(),
        properties: vec!["WorkspaceId".to_string(), "Path".to_string()],
        entity_id: false,
    };
    // Order-independent, comma-joined, sorted by name.
    assert_eq!(
        crate::key_index::declared_key_set_signature(&[key("ws_path"), key("name_parent")]),
        "name_parent,ws_path"
    );
    assert_eq!(crate::key_index::declared_key_set_signature(&[]), "");
    assert_eq!(
        crate::key_index::declared_key_set_signature(&[key("only")]),
        "only"
    );
}
