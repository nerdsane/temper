//! A record's fields reach Cedar as `context.<field>` unless the request
//! fills that key. The platform fills some keys itself on a move: the module
//! making the call, and the trigger that made it. A record field of the same
//! name must not stand in for one of them on a move the platform did not
//! fill it for: a caller who can write that field would pass a policy meant
//! only for the platform's own moves.

use super::*;
use temper_authz::platform_keys;

/// The value a policy trusts the platform to set.
const PLATFORM_ONLY: &str = "platform-only";

async fn cancel(state: &ServerState, id: &str) -> StatusCode {
    let (status, _) = customer_json(
        state,
        Method::POST,
        &format!("/tdata/Orders('{id}')/Temper.CancelOrder"),
        Some(serde_json::json!({ "Reason": "changed my mind" })),
    )
    .await;
    status
}

/// A user cancels two of their orders, one plain and one whose field `key`
/// holds [`PLATFORM_ONLY`], under a policy that lets only a call holding that
/// value at `context.<key>` cancel one.
async fn cancel_as_user(key: &str) -> (StatusCode, StatusCode) {
    let (state, _store) = build_default_state(77, &format!("platform-key-{key}"));
    let tenant = TenantId::default();
    for (id, fields) in [
        ("plain", serde_json::json!({ "Id": "plain" })),
        (
            "holds",
            serde_json::json!({ "Id": "holds", key: PLATFORM_ONLY }),
        ),
    ] {
        let created = state
            .get_or_create_tenant_entity(&tenant, "Order", id, fields)
            .await
            .expect("create the order");
        assert!(created.success, "create: {:?}", created.error);
        if id == "holds" {
            assert_eq!(
                created.state.fields[key], PLATFORM_ONLY,
                "the order holds {key}"
            );
        }
    }
    let policy = format!(
        r#"permit(principal, action == Action::"read", resource is Order);
           permit(principal, action == Action::"CancelOrder", resource is Order)
           when {{ context has {key} && context.{key} == "{PLATFORM_ONLY}" }};"#
    );
    state
        .authz
        .reload_tenant_policies(tenant.as_str(), &policy)
        .expect("install the policy");
    (cancel(&state, "plain").await, cancel(&state, "holds").await)
}

#[tokio::test]
async fn a_record_field_does_not_stand_in_for_a_platform_context_key() {
    let mut passed = Vec::new();
    for key in platform_keys::ALL {
        let (plain, holds) = cancel_as_user(key).await;
        // Control: without the field, the policy refuses the user.
        assert_eq!(
            plain,
            StatusCode::FORBIDDEN,
            "{key}: the policy must refuse a call that does not hold it"
        );
        if holds != StatusCode::FORBIDDEN {
            passed.push(format!("{key}: {holds}"));
        }
    }
    assert!(
        passed.is_empty(),
        "a record field passed for a platform context key: {passed:?}"
    );
}
