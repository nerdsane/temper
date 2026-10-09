//! Generic writes preserve effect-owned state through the production router and actor.
#[path = "field_write_ownership/data_only.rs"]
mod data_only;
#[path = "field_write_ownership/mod.rs"]
mod field_write_ownership;
#[path = "field_write_ownership/history.rs"]
mod history;

use axum::http::StatusCode;
use field_write_ownership::*;
use serde_json::json;
use temper_runtime::persistence::EventStore;
use temper_store_sim::SimEventStore;

#[tokio::test]
async fn patch_cannot_shadow_spec_counters() {
    let store = SimEventStore::no_faults(529);
    let state = state(store.clone(), false);
    create_and_advance(&state, "acme").await;
    let (status, body) = request(
        &state,
        "acme",
        "PATCH",
        ENTITY,
        json!({"units": 900, "retry_count": 901, "Notes": "writable"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["counters"]["units"], 1);
    assert_eq!(
        body["fields"]["units"], 1,
        "PATCH must not shadow authoritative counter: {body}"
    );
    assert_eq!(body["fields"]["retry_count"], 2);
    assert_eq!(body["fields"]["Notes"], "writable");
    let events = store.read_events("acme:Order:owned", 0).await.unwrap();
    let params = &events.last().unwrap().payload["params"];
    assert!(
        params.get("units").is_none(),
        "forged counter in journal: {params}"
    );
    assert!(params.get("retry_count").is_none());
    assert_eq!(params["Notes"], "writable");
}

#[tokio::test]
async fn create_cannot_seed_authoritative_mirrors() {
    let store = SimEventStore::no_faults(530);
    let state = state(store.clone(), false);
    let (status, body) = request(
        &state,
        "acme",
        "POST",
        "/tdata/Orders",
        json!({
            "id":"owned", "units":900, "approved":true, "reviewers":["forged"], "Notes":"writable"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let events = store.read_events("acme:Order:owned", 0).await.unwrap();
    let fields = &events[0].payload["initial_values"]["fields"];
    for key in ["units", "approved", "reviewers"] {
        assert!(
            fields.get(key).is_none(),
            "forged {key} in Created initial_values: {fields}"
        );
        assert!(events[0].payload["params"].get(key).is_none());
        assert!(body["fields"].get(key).is_none());
    }
    assert_eq!(fields["Notes"], "writable");
    assert_eq!(
        body["counters"],
        json!({}),
        "must not invent non-strict defaults"
    );
}

#[tokio::test]
async fn create_authorization_cannot_observe_forged_mirrors() {
    let store = SimEventStore::no_faults(531);
    let state = state(store.clone(), false);
    state
        .authz
        .reload_tenant_policies(
            "acme",
            r#"
        permit(principal, action, resource);
        forbid(principal, action == Action::"create", resource)
        when { resource has units || resource has approved || resource has reviewers };
    "#,
        )
        .unwrap();
    let (status, body) = request(
        &state,
        "acme",
        "POST",
        "/tdata/Orders",
        json!({
            "id":"owned", "units":900, "approved":true, "reviewers":["forged"], "Notes":"writable"
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "Cedar must see normalized fields, not ignored caller values: {body}"
    );
}

#[tokio::test]
async fn patch_put_preserve_owned_state_and_replay_query_views_across_seeds() {
    for seed in 529..545 {
        let (_guard, _clock, _ids) = temper_runtime::scheduler::install_deterministic_context(seed);
        let store = SimEventStore::no_faults(seed);
        let live = state(store.clone(), false);
        create_and_advance(&live, "acme").await;
        let owned = json!({
            "units":900, "Units":901, "retry_count":902, "RetryCount":903,
            "approved":false, "Approved":false, "pending_count":904, "PendingCount":905,
            "is_ready":false, "IsReady":false, "reviewers":["forged"], "Reviewers":["forged"],
            "Notes":"writable", "ordinary_array":[1,2,3], "keep_until_put":"old",
            "id":"forged", "Id":"forged", "status":"Forged", "Status":"Forged",
            "has_spec":false, "ctx_owner_status":"Privileged"
        });
        for (method, input) in [
            ("PATCH", owned.clone()),
            ("PUT", owned),
            ("PUT", json!({"Notes":"writable", "ordinary_array":[1,2,3]})),
        ] {
            let (status, body) = request(&live, "acme", method, ENTITY, input).await;
            assert_eq!(status, StatusCode::OK, "seed={seed}, {method}: {body}");
            assert_owned_view(&body);
            assert_eq!(body["fields"]["ordinary_array"], json!([1, 2, 3]));
        }
        let (_, current) = request(&live, "acme", "GET", ENTITY, json!({})).await;
        assert!(
            current["fields"].get("keep_until_put").is_none(),
            "PUT still replaces writable fields"
        );
        let events = store.read_events("acme:Order:owned", 0).await.unwrap();
        assert_eq!(events.len(), 5);
        for event in events.iter().skip(2) {
            for key in [
                "units",
                "Units",
                "RetryCount",
                "approved",
                "reviewers",
                "Reviewers",
                "pending_count",
                "is_ready",
                "Id",
                "Status",
                "has_spec",
                "ctx_owner_status",
            ] {
                assert!(
                    event.payload["params"].get(key).is_none(),
                    "{key} in journal: {event:?}"
                );
            }
        }
        let cold = state(store, false);
        cold.get_tenant_entity_state(
            &temper_runtime::tenant::TenantId::new("acme"),
            "Order",
            "owned",
        )
        .await
        .unwrap();
        for server in [&live, &cold] {
            let (status, body) = request(server, "acme", "GET", ENTITY, json!({})).await;
            assert_eq!(status, StatusCode::OK);
            assert_owned_view(&body);
            assert_eq!(body["fields"], current["fields"]);
            for (filter, count) in [
                (
                    "units%20eq%201%20and%20retry_count%20eq%202%20and%20approved%20eq%20true",
                    1,
                ),
                ("units%20eq%20900", 0),
                ("Units%20eq%20901", 0),
            ] {
                let (status, body) = request(
                    server,
                    "acme",
                    "GET",
                    &format!("/tdata/Orders?$filter={filter}"),
                    json!({}),
                )
                .await;
                assert_eq!(status, StatusCode::OK, "{body}");
                assert_eq!(body["value"].as_array().unwrap().len(), count, "{body}");
            }
        }
    }
}

fn assert_owned_view(body: &serde_json::Value) {
    assert_eq!(body["counters"], json!({"units":1,"retry_count":2}));
    assert_eq!(body["booleans"], json!({"approved":true}));
    assert_eq!(body["lists"], json!({"reviewers":["trusted"]}));
    let fields = &body["fields"];
    assert_eq!(fields["units"], 1);
    assert_eq!(fields["retry_count"], 2);
    assert_eq!(fields["approved"], true);
    assert_eq!(fields["reviewers"], json!(["trusted"]));
    assert_eq!(fields["Notes"], "writable");
    for key in [
        "Units",
        "RetryCount",
        "Approved",
        "Reviewers",
        "pending_count",
        "PendingCount",
        "is_ready",
        "IsReady",
        "has_spec",
        "ctx_owner_status",
    ] {
        assert!(
            fields.get(key).is_none(),
            "protected alias/absent value {key}: {body}"
        );
    }
    for key in ["Id", "id"] {
        assert_eq!(fields[key], "owned");
    }
    for key in ["Status", "status"] {
        assert_eq!(fields[key], "Draft");
    }
}

#[tokio::test]
async fn prospective_authorization_uses_owned_state_but_still_checks_writable_fields() {
    let store = SimEventStore::no_faults(546);
    let state = state(store.clone(), false);
    create_and_advance(&state, "acme").await;
    state
        .authz
        .reload_tenant_policies(
            "acme",
            r#"
        permit(principal, action, resource);
        forbid(principal, action == Action::"update", resource)
        when { resource.units != 1 || resource.approved != true || resource has Units };
        forbid(principal, action == Action::"update", resource)
        when { resource has Notes && resource.Notes == "denied" };
    "#,
        )
        .unwrap();
    for method in ["PATCH", "PUT"] {
        let (status, body) = request(
            &state,
            "acme",
            method,
            ENTITY,
            json!({"units":900,"Units":901,"approved":false,"Notes":"writable"}),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "normalized authority should be permitted: {body}"
        );
        let before = state
            .get_tenant_entity_state(
                &temper_runtime::tenant::TenantId::new("acme"),
                "Order",
                "owned",
            )
            .await
            .unwrap();
        let journal = store.read_events("acme:Order:owned", 0).await.unwrap();
        let (status, body) = request(
            &state,
            "acme",
            method,
            ENTITY,
            json!({"units":900,"Notes":"denied"}),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        let after = state
            .get_tenant_entity_state(
                &temper_runtime::tenant::TenantId::new("acme"),
                "Order",
                "owned",
            )
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_value(before.state).unwrap(),
            serde_json::to_value(after.state).unwrap()
        );
        assert_eq!(
            store
                .read_events("acme:Order:owned", 0)
                .await
                .unwrap()
                .len(),
            journal.len()
        );
    }
}

#[tokio::test]
async fn strict_refusal_and_tenant_authorization_are_unchanged() {
    let store = SimEventStore::no_faults(547);
    let strict = state(store.clone(), true);
    create_and_advance(&strict, "acme").await;
    for method in ["PATCH", "PUT"] {
        let (status, body) =
            request(&strict, "acme", method, ENTITY, json!({"Notes":"writable"})).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{body}");
    }
    assert_eq!(
        store
            .read_events("acme:Order:owned", 0)
            .await
            .unwrap()
            .len(),
        2
    );
    let store = SimEventStore::no_faults(548);
    let state = state(store.clone(), false);
    for tenant in ["acme", "other"] {
        create_and_advance(&state, tenant).await;
    }
    state
        .authz
        .reload_tenant_policies("other", "forbid(principal, action, resource);")
        .unwrap();
    for method in ["PATCH", "PUT"] {
        let (status, body) = request(
            &state,
            "other",
            method,
            ENTITY,
            json!({"Notes":"cross-tenant", "units":900}),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        let (status, body) = request(
            &state,
            "acme",
            method,
            ENTITY,
            json!({"Notes":"writable", "units":900}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["fields"]["units"], 1);
    }
    assert_eq!(
        store
            .read_events("other:Order:owned", 0)
            .await
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        store
            .read_events("acme:Order:owned", 0)
            .await
            .unwrap()
            .len(),
        4
    );
}
