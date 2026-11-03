//! Issue #519 V10: counts describe the authorized filtered collection, not the
//! cursor's remaining rows. Exercise the HTTP nextLink on each read backend.

use super::*;
use temper_runtime::scheduler::install_deterministic_context;

async fn seed_count_orders(state: &ServerState) {
    let tenant = TenantId::default();
    let ids = (0..12)
        .map(|index| format!("cnt-{index:02}"))
        .chain(["aaa-other", "zzz-other", "cnt-hidden"].map(str::to_string));
    for id in ids {
        // Explicit creation also synchronously projects the SQL-backed fixture.
        let response = state
            .get_or_create_tenant_entity(&tenant, "Order", &id, serde_json::json!({ "Id": id }))
            .await
            .expect("create count fixture");
        assert!(response.success, "create {id}: {:?}", response.error);
    }
    state
        .authz
        .reload_tenant_policies(
            tenant.as_str(),
            r#"
                permit(principal, action in [Action::"list", Action::"read"], resource is Order);
                forbid(principal, action == Action::"read", resource == Order::"cnt-hidden");
            "#,
        )
        .expect("install count fixture policy");
}

async fn assert_count_across_pages(state: &ServerState) {
    for (orderby, skip) in [("Id", 0), ("Id%20desc", 1)] {
        let mut expected: Vec<String> = (0..12).map(|index| format!("cnt-{index:02}")).collect();
        if skip != 0 {
            expected.reverse();
        }
        let expected: Vec<_> = expected.into_iter().skip(skip).collect();
        let mut path = format!(
            "/tdata/Orders?$filter=startswith(Id,'cnt-')&$top=5&$count=true&$orderby={orderby}&$skip={skip}"
        );
        let mut actual = Vec::new();
        let mut counts = Vec::new();
        for page in 0..3 {
            let (status, body) = get_json(state, &path).await;
            assert_eq!(status, StatusCode::OK, "page {page}: {body}");
            counts.push(body["@odata.count"].as_u64().expect("requested count"));
            let rows = body["value"].as_array().expect("page rows");
            let start = page * 5;
            let end = (start + 5).min(expected.len());
            let ids: Vec<_> = rows
                .iter()
                .map(|row| row["entity_id"].as_str().unwrap().to_string())
                .collect();
            assert_eq!(
                ids,
                expected[start..end],
                "page {page}: wrong membership/order"
            );
            actual.extend(ids);
            if page < 2 {
                let next = body["@odata.nextLink"].as_str().expect("next page");
                // nextLink is relative to the entity set's /tdata/ base URL.
                path = format!("/tdata/{next}");
            } else {
                assert!(
                    body.get("@odata.nextLink").is_none(),
                    "unexpected fourth page"
                );
            }
        }
        assert_eq!(
            actual, expected,
            "no skipped, duplicate or nonmatching rows"
        );
        assert_eq!(
            counts,
            vec![12, 12, 12],
            "count must precede skip/top/cursor"
        );
    }

    for paging in ["$top=0", "$top=5&$skip=20"] {
        let (status, body) = get_json(
            state,
            &format!("/tdata/Orders?$filter=startswith(Id,'cnt-')&$count=true&{paging}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["@odata.count"], 12,
            "empty page still counts the collection"
        );
        assert_eq!(body["value"], serde_json::json!([]));
        assert!(body.get("@odata.nextLink").is_none());
    }

    let (status, body) = get_json(
        state,
        "/tdata/Orders?$filter=startswith(Id,'absent-')&$top=5&$count=true&$orderby=Id",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["@odata.count"], 0);
    assert_eq!(body["value"], serde_json::json!([]));
    assert!(body.get("@odata.nextLink").is_none());
}

#[tokio::test]
async fn sim_count_is_stable_across_three_pages() {
    for seed in 0..16 {
        let (_guard, _clock, _ids) = install_deterministic_context(seed);
        let (state, _store) = build_default_state(seed, "pagination-count-sim");
        seed_count_orders(&state).await;
        assert_count_across_pages(&state).await;
    }
}

#[tokio::test]
async fn turso_count_is_stable_across_three_pages() {
    let dir = tempfile::tempdir().expect("isolated count database");
    let url = format!("file:{}", dir.path().join("count.db").display());
    let store = TursoEventStore::new(&url, None).await.unwrap();
    let state = build_turso_state("pagination-count-turso", store);
    seed_count_orders(&state).await;
    assert_count_across_pages(&state).await;
}

#[tokio::test]
#[ignore = "requires Docker for disposable PostgreSQL"]
async fn postgres_count_is_stable_across_three_pages() {
    use sqlx::postgres::PgPoolOptions;
    use temper_store_postgres::{PostgresEventStore, migration};
    use testcontainers::ImageExt;
    use testcontainers::runners::AsyncRunner;
    use testcontainers_modules::postgres::Postgres;

    let container = Postgres::default()
        .with_tag("16-alpine")
        .start()
        .await
        .expect("start PostgreSQL");
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let host = container.get_host().await.unwrap();
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&format!(
            "postgres://postgres:postgres@{host}:{port}/postgres"
        ))
        .await
        .unwrap();
    migration::run_migrations(&pool).await.unwrap();
    let store = PostgresEventStore::new(pool.clone());
    let (mut state, _) = build_default_state(519, "pagination-count-postgres");
    state.set_storage_stack(StorageStack::from_postgres(store));
    seed_count_orders(&state).await;
    assert_count_across_pages(&state).await;
    pool.close().await;
}
