//! A list read must decide each row from that row's fields, including fields a
//! policy reads through `context`.
//!
//! The Cedar engine copies every record field into the request context, so
//! `context.owner` is per-row data. These tests list `Orders` as user A while
//! one order belongs to A and one to B, on the simulated store and on Turso.
//! Both reads go through the query-plane list reader, which asks
//! `list_read_uniformity` whether one Cedar decision can stand for every row.

use super::*;

const USER_A: &str = "user-a";
const USER_B: &str = "user-b";
const ORDER_OF_A: &str = "ord-owned-by-a";
const ORDER_OF_B: &str = "ord-owned-by-b";

/// The forbid reads the owner through `context`; the permit allows everything else.
const FORBID_OTHER_OWNER_VIA_CONTEXT: &str = r#"
    permit(principal, action == Action::"list", resource is Order);
    permit(principal, action == Action::"read", resource);
    forbid(principal, action == Action::"read", resource)
    when { context has owner && context.owner != principal.id };
"#;

/// Only a permit, reading the owner through `context`.
const PERMIT_OWN_VIA_CONTEXT: &str = r#"
    permit(principal, action == Action::"list", resource is Order);
    permit(principal, action == Action::"read", resource)
    when { context has owner && context.owner == principal.id };
"#;

/// Control: the same forbid reading the owner through `resource`.
const FORBID_OTHER_OWNER_VIA_RESOURCE: &str = r#"
    permit(principal, action == Action::"list", resource is Order);
    permit(principal, action == Action::"read", resource);
    forbid(principal, action == Action::"read", resource)
    when { resource has owner && resource.owner != principal.id };
"#;

async fn seed_owned_orders(state: &ServerState, policy: &str) {
    let tenant = TenantId::default();
    for (id, owner) in [(ORDER_OF_A, USER_A), (ORDER_OF_B, USER_B)] {
        // Explicit creation also synchronously projects the SQL-backed catalog row.
        let response = state
            .get_or_create_tenant_entity(
                &tenant,
                "Order",
                id,
                serde_json::json!({ "Id": id, "owner": owner }),
            )
            .await
            .expect("create owned order");
        assert!(response.success, "create {id}: {:?}", response.error);
        assert_eq!(
            response.state.fields["owner"], owner,
            "{id} must carry its owner field"
        );
    }
    state
        .authz
        .reload_tenant_policies(tenant.as_str(), policy)
        .expect("install owner policy");
}

/// `GET /tdata/Orders` as an authenticated Customer; the listed ids, sorted.
async fn orders_listed_for(state: &ServerState, principal_id: &str) -> (StatusCode, Vec<String>) {
    let request = authenticate(
        Request::builder()
            .uri("/tdata/Orders")
            .body(Body::empty())
            .unwrap(),
        principal_id,
    );
    let response = build_router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
    let mut ids: Vec<String> = body["value"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|row| row["entity_id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    ids.sort();
    (status, ids)
}

type Listed = (&'static str, StatusCode, Vec<String>);

/// What user A's list returns under `policy`, on the sim store and on Turso.
async fn listed_for_user_a(name: &str, policy: &str) -> Vec<Listed> {
    let (sim, _sim_store) = build_default_state(541, &format!("{name}-sim"));
    seed_owned_orders(&sim, policy).await;
    let on_sim = orders_listed_for(&sim, USER_A).await;

    let dir = tempfile::tempdir().expect("isolated owner database");
    let url = format!("file:{}", dir.path().join("owner.db").display());
    let store = TursoEventStore::new(&url, None).await.unwrap();
    let turso = build_turso_state(&format!("{name}-turso"), store);
    seed_owned_orders(&turso, policy).await;
    let on_turso = orders_listed_for(&turso, USER_A).await;

    vec![
        ("sim", on_sim.0, on_sim.1),
        ("turso", on_turso.0, on_turso.1),
    ]
}

fn only_order_of_a() -> Vec<Listed> {
    let own = vec![ORDER_OF_A.to_string()];
    vec![
        ("sim", StatusCode::OK, own.clone()),
        ("turso", StatusCode::OK, own),
    ]
}

#[tokio::test]
async fn context_owner_forbid_hides_other_owners_rows_from_list() {
    let listed =
        listed_for_user_a("list-context-owner-forbid", FORBID_OTHER_OWNER_VIA_CONTEXT).await;
    assert_eq!(
        listed,
        only_order_of_a(),
        "user A listed with a context.owner forbid: (backend, status, ids)"
    );
}

#[tokio::test]
async fn context_owner_permit_shows_own_rows_in_list() {
    let listed = listed_for_user_a("list-context-owner-permit", PERMIT_OWN_VIA_CONTEXT).await;
    assert_eq!(
        listed,
        only_order_of_a(),
        "user A listed with a context.owner permit: (backend, status, ids)"
    );
}

#[tokio::test]
async fn resource_owner_forbid_hides_other_owners_rows_from_list() {
    let listed = listed_for_user_a(
        "list-resource-owner-forbid",
        FORBID_OTHER_OWNER_VIA_RESOURCE,
    )
    .await;
    assert_eq!(
        listed,
        only_order_of_a(),
        "user A listed with a resource.owner forbid (control): (backend, status, ids)"
    );
}
