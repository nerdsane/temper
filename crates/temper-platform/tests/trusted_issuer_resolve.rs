//! Integration test for ARN-255 step 1: the kernel resolver verifies ES256
//! JWTs against a registered `TrustedIssuer` and rejects bad ones.
//!
//! Exercises the real path end to end: a real `PlatformState` with the agent
//! specs bootstrapped, a real `TrustedIssuer` entity seeded through the normal
//! dispatch path with a real P-256 JWKS, real ES256 tokens minted here, and
//! the real `IdentityResolver::resolve` mapping verified claims to a principal.

use std::collections::BTreeMap;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};

use temper_platform::{PlatformState, bootstrap_agent_specs, bootstrap_system_tenant};
use temper_runtime::tenant::TenantId;
use temper_server::identity::IdentityResolver;
use temper_server::request_context::AgentContext;

const ISSUER: &str = "https://issuer.e2e.local";
const AUD: &str = "temper-e2e";

fn b64(v: &serde_json::Value) -> String {
    URL_SAFE_NO_PAD.encode(serde_json::to_vec(v).unwrap())
}

/// Mint a signed ES256 token from header + claims JSON.
fn mint(sk: &SigningKey, header: serde_json::Value, claims: serde_json::Value) -> String {
    let signing_input = format!("{}.{}", b64(&header), b64(&claims));
    let sig: Signature = sk.sign(signing_input.as_bytes());
    format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(sig.to_bytes()))
}

/// Build a single-key JWKS JSON document for a signing key.
fn jwks_json(sk: &SigningKey, kid: &str) -> String {
    let vk = sk.verifying_key();
    let pt = vk.to_encoded_point(false);
    let x = URL_SAFE_NO_PAD.encode(pt.x().unwrap());
    let y = URL_SAFE_NO_PAD.encode(pt.y().unwrap());
    serde_json::json!({
        "keys": [{ "kty": "EC", "crv": "P-256", "kid": kid, "x": x, "y": y }]
    })
    .to_string()
}

fn header() -> serde_json::Value {
    serde_json::json!({ "alg": "ES256", "kid": "k1", "typ": "JWT" })
}

/// A valid contributor token: far-future exp so it is valid regardless of the
/// wall clock, nbf in the distant past.
fn contributor_claims() -> serde_json::Value {
    serde_json::json!({
        "iss": ISSUER,
        "sub": "human-e2e",
        "aud": AUD,
        "client_id": "kc_agent_e2e",
        "agent_type": "contributor",
        "grant_id": "grant-e2e",
        "auth_generation": 3,
        "nbf": 0,
        "exp": 4_102_444_800i64, // year 2100
    })
}

async fn state_with_issuer(sk: &SigningKey) -> PlatformState {
    let state = PlatformState::new(None);
    let cache = BTreeMap::new();
    bootstrap_system_tenant(&state, &cache);
    bootstrap_agent_specs(&state, "default", false, &cache);

    let tenant = TenantId::new("default");
    let ctx = AgentContext::system();
    state
        .server
        .dispatch_tenant_action(
            &tenant,
            "TrustedIssuer",
            ISSUER,
            "RegisterIssuer",
            serde_json::json!({
                "issuer": ISSUER,
                "jwks_json": jwks_json(sk, "k1"),
                "audience": AUD,
                "algorithms": "ES256",
                "description": "e2e issuer",
                "created_by": "e2e",
            }),
            &ctx,
        )
        .await
        .expect("register issuer");
    state
}

#[tokio::test]
async fn valid_token_resolves_to_verified_agent_acting_for_human() {
    let sk = SigningKey::from_slice(&[7u8; 32]).unwrap();
    let state = state_with_issuer(&sk).await;
    let token = mint(&sk, header(), contributor_claims());

    let resolver = IdentityResolver::new();
    let id = resolver
        .resolve(&state.server, &TenantId::new("default"), &token)
        .await
        .expect("valid token should resolve");

    assert!(id.verified);
    assert!(id.from_jwt);
    assert_eq!(id.agent_instance_id, "kc_agent_e2e");
    assert_eq!(id.agent_type_name, "contributor");
    assert_eq!(id.acting_for.as_deref(), Some("human-e2e"));
}

#[tokio::test]
async fn token_signed_by_unknown_key_is_rejected() {
    let sk = SigningKey::from_slice(&[7u8; 32]).unwrap();
    let state = state_with_issuer(&sk).await;

    // Sign with a different key than the registered JWKS.
    let rogue = SigningKey::from_slice(&[9u8; 32]).unwrap();
    let token = mint(&rogue, header(), contributor_claims());

    let resolver = IdentityResolver::new();
    let id = resolver
        .resolve(&state.server, &TenantId::new("default"), &token)
        .await;
    assert!(id.is_none(), "rogue-key token must not resolve");
}

#[tokio::test]
async fn expired_token_is_rejected() {
    let sk = SigningKey::from_slice(&[7u8; 32]).unwrap();
    let state = state_with_issuer(&sk).await;

    let mut claims = contributor_claims();
    claims["exp"] = serde_json::json!(100); // 1970 — long expired vs wall clock
    let token = mint(&sk, header(), claims);

    let resolver = IdentityResolver::new();
    let id = resolver
        .resolve(&state.server, &TenantId::new("default"), &token)
        .await;
    assert!(id.is_none(), "expired token must not resolve");
}

#[tokio::test]
async fn unregistered_issuer_is_rejected() {
    let sk = SigningKey::from_slice(&[7u8; 32]).unwrap();
    let state = state_with_issuer(&sk).await;

    let mut claims = contributor_claims();
    claims["iss"] = serde_json::json!("https://not-registered.example");
    let token = mint(&sk, header(), claims);

    let resolver = IdentityResolver::new();
    let id = resolver
        .resolve(&state.server, &TenantId::new("default"), &token)
        .await;
    assert!(
        id.is_none(),
        "token from an unregistered issuer must not resolve"
    );
}

#[tokio::test]
async fn tampered_payload_is_rejected() {
    let sk = SigningKey::from_slice(&[7u8; 32]).unwrap();
    let state = state_with_issuer(&sk).await;

    let token = mint(&sk, header(), contributor_claims());
    // Swap the payload for one escalating agent_type, keep the original signature.
    let mut parts: Vec<&str> = token.split('.').collect();
    let forged = b64(&serde_json::json!({
        "iss": ISSUER, "aud": AUD, "client_id": "kc_agent_e2e",
        "agent_type": "owner", "exp": 4_102_444_800i64,
    }));
    parts[1] = &forged;
    let tampered = parts.join(".");

    let resolver = IdentityResolver::new();
    let id = resolver
        .resolve(&state.server, &TenantId::new("default"), &tampered)
        .await;
    assert!(id.is_none(), "tampered token must not resolve");
}

#[tokio::test]
async fn suspended_issuer_stops_resolving() {
    let sk = SigningKey::from_slice(&[7u8; 32]).unwrap();
    let state = state_with_issuer(&sk).await;
    let token = mint(&sk, header(), contributor_claims());
    let tenant = TenantId::new("default");

    // Valid before suspension.
    let resolver = IdentityResolver::new();
    assert!(
        resolver
            .resolve(&state.server, &tenant, &token)
            .await
            .is_some(),
        "token should resolve while issuer is Active"
    );

    // Suspend the issuer.
    let ctx = AgentContext::system();
    state
        .server
        .dispatch_tenant_action(
            &tenant,
            "TrustedIssuer",
            ISSUER,
            "SuspendIssuer",
            serde_json::json!({}),
            &ctx,
        )
        .await
        .expect("suspend issuer");

    // A fresh resolver (empty cache) must now reject the same token.
    let fresh = IdentityResolver::new();
    assert!(
        fresh
            .resolve(&state.server, &tenant, &token)
            .await
            .is_none(),
        "token must not resolve once its issuer is Suspended"
    );
}

#[tokio::test]
async fn human_token_resolves_to_customer_with_role() {
    let sk = SigningKey::from_slice(&[7u8; 32]).unwrap();
    let state = state_with_issuer(&sk).await;

    // A human token: `sub` + `role`, no `agent_type`/`client_id`.
    let claims = serde_json::json!({
        "iss": ISSUER, "aud": AUD, "sub": "human-owner",
        "role": "owner", "auth_generation": 0,
        "nbf": 0, "exp": 4_102_444_800i64,
    });
    let token = mint(&sk, header(), claims);

    let resolver = IdentityResolver::new();
    let id = resolver
        .resolve(&state.server, &TenantId::new("default"), &token)
        .await
        .expect("human token should resolve");

    assert!(id.from_jwt);
    assert!(id.is_human);
    assert_eq!(id.agent_instance_id, "human-owner");
    assert_eq!(id.role.as_deref(), Some("owner"));
    assert!(id.acting_for.is_none());
}

#[tokio::test]
async fn bumping_generation_invalidates_older_tokens() {
    let sk = SigningKey::from_slice(&[7u8; 32]).unwrap();
    let state = state_with_issuer(&sk).await;
    let tenant = TenantId::new("default");

    // Token minted at generation 0 (contributor_claims stamps auth_generation=3;
    // build one at gen 0 for clarity).
    let gen0 = {
        let mut c = contributor_claims();
        c["auth_generation"] = serde_json::json!(0);
        mint(&sk, header(), c)
    };

    // Valid before any bump.
    let resolver = IdentityResolver::new();
    assert!(
        resolver
            .resolve(&state.server, &tenant, &gen0)
            .await
            .is_some(),
        "gen-0 token valid before sign-out-everywhere"
    );

    // Sign out everywhere: bump the human's generation to 1. The generation is
    // keyed on the human `sub`, which contributor_claims sets to "human-e2e".
    let ctx = AgentContext::system();
    state
        .server
        .dispatch_tenant_action(
            &tenant,
            "PrincipalGeneration",
            "human-e2e",
            "BumpGeneration",
            serde_json::json!({}),
            &ctx,
        )
        .await
        .expect("bump generation");

    // A fresh resolver must now reject the gen-0 token (0 < current 1)...
    let fresh = IdentityResolver::new();
    assert!(
        fresh.resolve(&state.server, &tenant, &gen0).await.is_none(),
        "gen-0 token must be rejected after the generation is bumped"
    );

    // ...but a token minted at the new generation (1) still resolves.
    let gen1 = {
        let mut c = contributor_claims();
        c["auth_generation"] = serde_json::json!(1);
        mint(&sk, header(), c)
    };
    assert!(
        fresh.resolve(&state.server, &tenant, &gen1).await.is_some(),
        "a token minted at the current generation must still resolve"
    );
}

#[tokio::test]
async fn revoking_a_grant_stops_the_agents_token_at_the_kernel() {
    let sk = SigningKey::from_slice(&[7u8; 32]).unwrap();
    let state = state_with_issuer(&sk).await;
    let tenant = TenantId::new("default");
    let token = mint(&sk, header(), contributor_claims()); // grant_id = "grant-e2e"

    // Valid while the grant is live.
    let resolver = IdentityResolver::new();
    assert!(
        resolver
            .resolve(&state.server, &tenant, &token)
            .await
            .is_some(),
        "agent token resolves while its grant is live"
    );

    // Revoke the grant: bump the counter keyed by grant_id.
    let ctx = AgentContext::system();
    state
        .server
        .dispatch_tenant_action(
            &tenant,
            "PrincipalGeneration",
            "grant-e2e",
            "BumpGeneration",
            serde_json::json!({}),
            &ctx,
        )
        .await
        .expect("revoke grant");

    // A fresh resolver must now reject it — no waiting for token expiry.
    let fresh = IdentityResolver::new();
    assert!(
        fresh
            .resolve(&state.server, &tenant, &token)
            .await
            .is_none(),
        "a revoked grant must stop resolving at the kernel immediately"
    );
}

#[tokio::test]
async fn a_rejected_token_never_materialises_a_trusted_issuer() {
    let sk = SigningKey::from_slice(&[7u8; 32]).unwrap();
    let state = state_with_issuer(&sk).await;
    let tenant = TenantId::new("default");

    // `iss` is attacker-chosen and read before any signature check, and entity
    // reads spawn on demand — so this used to persist an Active TrustedIssuer
    // row with empty fields, unauthenticated, for any string the caller chose.
    let ghost = "https://ghost.attacker.example";
    let mut claims = contributor_claims();
    claims["iss"] = serde_json::json!(ghost);
    let token = mint(&sk, header(), claims);

    let resolver = IdentityResolver::new();
    assert!(
        resolver
            .resolve(&state.server, &tenant, &token)
            .await
            .is_none(),
        "a token from an unregistered issuer must not resolve"
    );
    assert!(
        !state.server.entity_exists(&tenant, "TrustedIssuer", ghost),
        "rejecting the token must not leave a TrustedIssuer row behind"
    );
}

#[tokio::test]
async fn resolving_does_not_materialise_generation_rows() {
    let sk = SigningKey::from_slice(&[7u8; 32]).unwrap();
    let state = state_with_issuer(&sk).await;
    let tenant = TenantId::new("default");
    let token = mint(&sk, header(), contributor_claims());

    let resolver = IdentityResolver::new();
    assert!(
        resolver
            .resolve(&state.server, &tenant, &token)
            .await
            .is_some()
    );

    // Never-revoked principals read as generation 0 without persisting a
    // counter row per subject and per grant.
    for key in ["human-e2e", "grant-e2e"] {
        assert!(
            !state
                .server
                .entity_exists(&tenant, "PrincipalGeneration", key),
            "resolving must not create a PrincipalGeneration row for {key}"
        );
    }
}
