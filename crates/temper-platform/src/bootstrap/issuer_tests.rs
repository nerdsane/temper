//! A start applies the environment's issuer keys only while the stored issuer
//! still takes its keys from the environment. Each start is a fresh server on
//! the same store, so what the issuer keeps is what survives a restart.
use super::*;
use temper_store_sim::SimEventStore;

const TENANT: &str = "issuer-tenant";
const ISSUER: &str = "https://issuer.example.test";
const AUDIENCE: &str = "https://audience.example.test";

fn jwks(kid: &str) -> String {
    format!(r#"{{"keys":[{{"kty":"EC","crv":"P-256","kid":"{kid}","x":"x","y":"y"}}]}}"#)
}

fn persistence_id() -> String {
    format!("{TENANT}:TrustedIssuer:{ISSUER}")
}

/// A server over `store` that has loaded nothing yet: a restart.
async fn server_on(store: &SimEventStore) -> PlatformState {
    let mut state = PlatformState::new(None);
    state
        .server
        .set_storage_stack(temper_server::StorageStack::from_sim(store.clone(), None));
    bootstrap_identity_specs(&state, TENANT)
        .await
        .expect("identity specs should register");
    state
}

/// One start with the environment holding key set `kid`.
async fn start(store: &SimEventStore, kid: &str) -> Result<IssuerBootstrapOutcome, String> {
    let state = server_on(store).await;
    apply_environment_issuer(&state, TENANT, ISSUER, &jwks(kid), AUDIENCE, "ES256").await
}

async fn started(store: &SimEventStore, kid: &str) {
    let outcome = start(store, kid).await.expect("environment issuer applies");
    assert_eq!(outcome, IssuerBootstrapOutcome::Registered);
}

async fn act(store: &SimEventStore, action: &str, params: serde_json::Value) {
    let state = server_on(store).await;
    let response = state
        .server
        .dispatch_tenant_action(
            &temper_runtime::tenant::TenantId::new(TENANT),
            "TrustedIssuer",
            ISSUER,
            action,
            params,
            &temper_server::request_context::AgentContext::system(),
        )
        .await
        .expect("dispatch reaches the issuer");
    assert!(response.success, "{action} failed: {:?}", response.error);
}

/// The issuer as a fresh server reads it from the store.
async fn stored(store: &SimEventStore) -> temper_server::entity_actor::EntityState {
    let state = server_on(store).await;
    stored_trusted_issuer(
        &state,
        &temper_runtime::tenant::TenantId::new(TENANT),
        ISSUER,
    )
    .await
    .expect("the store reads")
    .expect("the issuer is stored")
}

async fn stored_jwks(store: &SimEventStore) -> String {
    stored(store).await.fields["jwks_json"]
        .as_str()
        .expect("jwks_json is a string")
        .to_string()
}

async fn rotated_to(store: &SimEventStore, kid: &str) {
    act(
        store,
        "RotateIssuerKeys",
        serde_json::json!({ "jwks_json": jwks(kid) }),
    )
    .await;
}

#[tokio::test]
async fn a_changed_environment_key_applies_to_an_issuer_the_environment_holds() {
    let store = SimEventStore::no_faults(0);
    started(&store, "kid-a").await;
    assert_eq!(stored_jwks(&store).await, jwks("kid-a"));

    started(&store, "kid-b").await;
    assert_eq!(stored_jwks(&store).await, jwks("kid-b"));
    assert_ne!(
        stored(&store).await.booleans.get("keys_rotated"),
        Some(&true)
    );
}

#[tokio::test]
async fn a_rotated_issuer_key_is_kept_after_a_restart() {
    let store = SimEventStore::no_faults(0);
    started(&store, "kid-a").await;
    rotated_to(&store, "kid-b").await;
    assert_eq!(
        stored(&store).await.booleans.get("keys_rotated"),
        Some(&true)
    );

    started(&store, "kid-a").await;
    assert_eq!(stored_jwks(&store).await, jwks("kid-b"));
    started(&store, "kid-c").await;
    assert_eq!(stored_jwks(&store).await, jwks("kid-b"));
    assert_eq!(
        stored(&store).await.booleans.get("keys_rotated"),
        Some(&true)
    );
}

#[tokio::test]
async fn a_suspended_or_revoked_issuer_is_left_as_it_is_after_a_restart() {
    for (action, status) in [("SuspendIssuer", "Suspended"), ("RevokeIssuer", "Revoked")] {
        let store = SimEventStore::no_faults(0);
        started(&store, "kid-a").await;
        act(&store, action, serde_json::json!({})).await;

        started(&store, "kid-b").await;
        let issuer = stored(&store).await;
        assert_eq!(issuer.status, status);
        assert_eq!(
            issuer.fields["jwks_json"].as_str(),
            Some(jwks("kid-a").as_str())
        );
    }
}

#[tokio::test]
async fn a_start_that_cannot_read_the_issuer_leaves_its_rotated_key() {
    let store = SimEventStore::no_faults(0);
    started(&store, "kid-a").await;
    rotated_to(&store, "kid-b").await;

    store.fail_next_reads(&persistence_id(), 1);
    assert!(
        start(&store, "kid-a").await.is_err(),
        "a start that cannot read the stored issuer does not register over it"
    );

    assert_eq!(stored_jwks(&store).await, jwks("kid-b"));
    assert_eq!(
        stored(&store).await.booleans.get("keys_rotated"),
        Some(&true)
    );
}
