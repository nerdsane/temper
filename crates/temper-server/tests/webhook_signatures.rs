//! Webhooks over HTTP, as a provider and a receiver see them.
//!
//! Inbound: a provider's signed request to `/webhooks/{tenant}/{path}` moves
//! the record it names, and a request whose signature is missing, wrong, or
//! signed too long ago is refused with 401 and moves nothing. The signed
//! requests are the providers' own published examples where they publish one
//! (GitHub, Svix), and values computed independently with Python's `hmac`
//! where they do not (Stripe, a body HMAC, Temper's own scheme). Each record
//! is read back over OData.
//!
//! The receiver hands the action what the webhook declares it reads from the
//! body, the query and the headers, and refuses a request it cannot read with
//! 400, but only once its signature has been checked.
//!
//! Outgoing: a transition is posted to every configured webhook whose filters
//! match it, with its payload template filled in and its headers expanded from
//! the environment; a refused action is not posted.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use temper_authz::{AuthenticatedRequestContext, SecurityContext};
use temper_runtime::ActorSystem;
use temper_runtime::scheduler::{RealIdGen, SimClock, install_sim_context};
use temper_runtime::tenant::TenantId;
use temper_server::registry::{
    EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus,
};
use temper_server::secrets::vault::SecretsVault;
use temper_server::webhooks::WebhookDispatcher;
use temper_server::{ServerState, build_router};
use temper_spec::csdl::parse_csdl;
use tower::ServiceExt;

/// The tenant. Temper's own published signature below signs
/// `POST /webhooks/t/p?x=1`, so the tenant is `t` and that webhook's path `p`.
const TENANT: &str = "t";

const DELIVERY_IOA: &str = r#"
[automaton]
name = "Delivery"
states = ["Waiting", "Received", "Signed"]
initial = "Waiting"

[[action]]
name = "Receive"
kind = "input"
from = ["Waiting"]
to = "Received"

[[action]]
name = "Sign"
kind = "input"
from = ["Received"]
to = "Signed"

[[action]]
name = "Record"
kind = "input"
from = ["Waiting"]
to = "Received"
params = [{ name = "amount", type = "int" }, "event", "delivery", "source", "first_item"]

[[webhook]]
name = "kernel"
path = "p"
action = "Receive"
hmac_secret = "{secret:KERNEL_SECRET}"
hmac_header = "x-webhook-signature"
entity_id = "query.x"

[[webhook]]
name = "github"
path = "github"
action = "Receive"
signature = "github"
hmac_secret = "{secret:GITHUB_SECRET}"
entity_id = "query.delivery"

[[webhook]]
name = "hmac"
path = "hmac"
action = "Receive"
signature = "hmac"
hmac_secret = "{secret:HMAC_SECRET}"
hmac_header = "x-shopify-hmac-sha256"
entity_id = "query.delivery"

[[webhook]]
name = "stripe"
path = "stripe"
action = "Receive"
signature = "stripe"
hmac_secret = "{secret:STRIPE_SECRET}"
entity_id = "query.delivery"

[[webhook]]
name = "standard"
path = "standard"
action = "Receive"
signature = "standard_webhooks"
hmac_secret = "{secret:SVIX_SECRET}"
entity_id = "query.delivery"

[[webhook]]
name = "token"
path = "token"
action = "Receive"
signature = "token"
hmac_secret = "{secret:GITLAB_TOKEN}"
hmac_header = "x-gitlab-token"
entity_id = "query.delivery"

[[webhook]]
name = "unresolved"
path = "unresolved"
action = "Receive"
signature = "token"
hmac_secret = "{secret:NOT_IN_THE_VAULT}"
hmac_header = "x-gitlab-token"
entity_id = "query.delivery"

[[webhook]]
name = "event"
path = "event"
action = "Record"
signature = "hmac"
hmac_secret = "{secret:HMAC_SECRET}"
hmac_header = "x-signature"
entity_id = "body.data.object.reference"

[webhook.extract]
amount = "body.data.object.amount"
event = "body.id"
delivery = "header.x-delivery"
source = "query.source"
first_item = "body.data.items.0.sku"
"#;

const DELIVERY_CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Hook" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Delivery">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Deliveries" EntityType="Temper.Hook.Delivery"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

/// Each webhook may take its one action; the desk that creates and reads the
/// records may do anything.
const POLICY: &str = r#"
permit(principal == Agent::"webhook:kernel", action == Action::"Receive", resource is Delivery);
permit(principal == Agent::"webhook:github", action == Action::"Receive", resource is Delivery);
permit(principal == Agent::"webhook:hmac", action == Action::"Receive", resource is Delivery);
permit(principal == Agent::"webhook:stripe", action == Action::"Receive", resource is Delivery);
permit(principal == Agent::"webhook:standard", action == Action::"Receive", resource is Delivery);
permit(principal == Agent::"webhook:token", action == Action::"Receive", resource is Delivery);
permit(principal == Agent::"webhook:unresolved", action == Action::"Receive", resource is Delivery);
permit(principal == Agent::"webhook:event", action == Action::"Record", resource is Delivery);
permit(principal == Agent::"delivery-desk", action, resource);
"#;

// Temper's own scheme: HMAC-SHA256(key = "kernel-secret",
// "POST\n/webhooks/t/p?x=1\n{\"a\":1}"), computed with Python's hmac module.
const KERNEL_SECRET: &str = "kernel-secret";
const KERNEL_PATH: &str = "/webhooks/t/p?x=1";
const KERNEL_BODY: &str = r#"{"a":1}"#;
const KERNEL_SIGNATURE: &str = "b401150004071493727748c0e539982a6bdb009ee0e18caee8c53439c638795e";

// GitHub publishes this example:
// https://docs.github.com/en/webhooks/using-webhooks/validating-webhook-deliveries#testing-the-webhook-payload-validation
const GITHUB_SECRET: &str = "It's a Secret to Everybody";
const GITHUB_BODY: &str = "Hello, World!";
const GITHUB_SIGNATURE: &str =
    "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";

// A body HMAC as Shopify, Linear and most others send it:
// HMAC-SHA256(key = "shpss_test", body), computed with Python's hmac module.
const HMAC_SECRET: &str = "shpss_test";
const HMAC_BODY: &str = r#"{"id":820982911946154508}"#;
const HMAC_HEX: &str = "dc9c573bc2ee979da20f9faa6f5147b685caf2f0f03db5e98bc781fb21139392";
const HMAC_BASE64: &str = "3JxXO8Lul52iD5+qb1FHtoXK8vDwPbXpi8eB+yETk5I=";

// Stripe documents the format (https://docs.stripe.com/webhooks#verify-manually)
// but publishes no secret for its example, so this is HMAC-SHA256(key =
// "whsec_test_secret", "1492774577." + body), computed with Python's hmac.
const STRIPE_SECRET: &str = "whsec_test_secret";
const STRIPE_T: i64 = 1_492_774_577;
const STRIPE_BODY: &str = r#"{"id":"evt_test_webhook","object":"event"}"#;
const STRIPE_V1: &str = "88a022085c6bdb887b02cb26ff76dd681234d9675c0f22844059f55552a8883a";
const STRIPE_V0: &str = "6ffbb59b2300aae63f272406069a9788598b792a944a07aba816edb039989a39";

// Svix publishes this example ("Example signatures"):
// https://docs.svix.com/receiving/verifying-payloads/how-manual
const SVIX_SECRET: &str = "whsec_plJ3nmyCDGBKInavdOK15jsl";
const SVIX_ID: &str = "msg_loFOjxBNrRLzqYUf";
const SVIX_T: i64 = 1_731_705_121;
const SVIX_BODY: &str = r#"{"event_type":"ping","data":{"success":true}}"#;
const SVIX_SIGNATURE: &str = "v1,rAvfW3dJ/X/qxhsaXPOyyCGmRKsaKWcsNccKXlIktD0=";

// A sender that cannot sign sends the secret itself (GitLab's X-Gitlab-Token).
const GITLAB_TOKEN: &str = "s3cret";

/// How far a signed time may be from the server's clock: five minutes, the
/// default of Stripe's and the Standard Webhooks libraries.
const TOLERANCE_SECONDS: i64 = 300;

/// The server's clock, set to the moment each provider example was signed.
struct ProviderClock(AtomicI64);

impl SimClock for ProviderClock {
    fn now(&self) -> DateTime<Utc> {
        DateTime::from_timestamp(self.0.load(Ordering::SeqCst), 0).expect("a valid time")
    }

    fn tick(&self) -> u64 {
        0
    }
}

fn build_state() -> ServerState {
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        TENANT,
        parse_csdl(DELIVERY_CSDL_XML).expect("CSDL parses"),
        DELIVERY_CSDL_XML.to_string(),
        &[("Delivery", DELIVERY_IOA)],
    );
    let state = ServerState::from_registry(ActorSystem::new("webhook-signatures"), registry);
    // HTTP writes wait for the spec's verification, which a test never runs.
    state
        .registry
        .write()
        .expect("the registry lock is free")
        .set_verification_status(
            &TenantId::new(TENANT),
            "Delivery",
            VerificationStatus::Completed(EntityVerificationResult {
                all_passed: true,
                levels: vec![EntityLevelSummary {
                    level: "L0 SMT".to_string(),
                    passed: true,
                    summary: "OK".to_string(),
                    details: None,
                }],
                verified_at: "2026-10-28T00:00:00Z".to_string(),
            }),
        );
    let vault = SecretsVault::new(&[7u8; 32]);
    for (key, value) in [
        ("KERNEL_SECRET", KERNEL_SECRET),
        ("GITHUB_SECRET", GITHUB_SECRET),
        ("HMAC_SECRET", HMAC_SECRET),
        ("STRIPE_SECRET", STRIPE_SECRET),
        ("SVIX_SECRET", SVIX_SECRET),
        ("GITLAB_TOKEN", GITLAB_TOKEN),
    ] {
        vault.cache_secret(TENANT, key, value.to_string());
    }
    let state = state.with_secrets_vault(vault);
    state
        .authz
        .reload_tenant_policies(TENANT, POLICY)
        .expect("the test policy parses");
    state
}

async fn send(state: &ServerState, request: Request<Body>) -> (StatusCode, String) {
    let response = build_router(state.clone())
        .oneshot(request)
        .await
        .expect("the router answers");
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("the body reads");
    (status, String::from_utf8_lossy(&body).into_owned())
}

/// A request from the desk that creates and reads the records.
fn as_desk(mut request: Request<Body>) -> Request<Body> {
    request
        .extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::new(TENANT),
            SecurityContext::from_resolved_identity("delivery-desk", "test-agent", None),
        ));
    request
}

async fn create_delivery(state: &ServerState, id: &str) {
    let request = Request::post("/tdata/Deliveries")
        .header("content-type", "application/json")
        .body(Body::from(json!({ "id": id }).to_string()))
        .expect("the request builds");
    let (status, body) = send(state, as_desk(request)).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "delivery {id} is created: {body}"
    );
}

/// The record, read over OData.
async fn read_delivery(state: &ServerState, id: &str) -> Value {
    let request = Request::get(format!("/tdata/Deliveries('{id}')"))
        .body(Body::empty())
        .expect("the request builds");
    let (status, body) = send(state, as_desk(request)).await;
    assert_eq!(status, StatusCode::OK, "delivery {id} reads: {body}");
    serde_json::from_str(&body).expect("the record is JSON")
}

/// The record's status, read over OData.
async fn delivery_status(state: &ServerState, id: &str) -> String {
    read_delivery(state, id).await["status"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// One request a provider sends.
struct Delivery {
    what: &'static str,
    /// The record the request names.
    record: &'static str,
    method: &'static str,
    uri: String,
    headers: Vec<(&'static str, String)>,
    body: &'static str,
    /// The server's clock when the request arrives, for signed timestamps.
    at: Option<i64>,
}

impl Delivery {
    fn request(&self) -> Request<Body> {
        let mut builder = Request::builder().method(self.method).uri(&self.uri);
        for (name, value) in &self.headers {
            builder = builder.header(*name, value);
        }
        builder
            .body(Body::from(self.body))
            .expect("the request builds")
    }
}

fn kernel(what: &'static str, record: &'static str, method: &'static str) -> Delivery {
    Delivery {
        what,
        record,
        method,
        uri: KERNEL_PATH.to_string(),
        headers: vec![("x-webhook-signature", KERNEL_SIGNATURE.to_string())],
        body: KERNEL_BODY,
        at: None,
    }
}

fn github(what: &'static str, record: &'static str) -> Delivery {
    Delivery {
        what,
        record,
        method: "POST",
        uri: format!("/webhooks/t/github?delivery={record}"),
        headers: vec![("X-Hub-Signature-256", GITHUB_SIGNATURE.to_string())],
        body: GITHUB_BODY,
        at: None,
    }
}

fn hmac(what: &'static str, record: &'static str, signature: &str) -> Delivery {
    Delivery {
        what,
        record,
        method: "POST",
        uri: format!("/webhooks/t/hmac?delivery={record}"),
        headers: vec![("X-Shopify-Hmac-Sha256", signature.to_string())],
        body: HMAC_BODY,
        at: None,
    }
}

fn stripe(what: &'static str, record: &'static str, header: String, at: i64) -> Delivery {
    Delivery {
        what,
        record,
        method: "POST",
        uri: format!("/webhooks/t/stripe?delivery={record}"),
        headers: vec![("Stripe-Signature", header)],
        body: STRIPE_BODY,
        at: Some(at),
    }
}

fn stripe_signed() -> String {
    format!("t={STRIPE_T},v1={STRIPE_V1},v0={STRIPE_V0}")
}

/// Standard Webhooks headers under `prefix` (`webhook` or `svix`).
fn standard(
    what: &'static str,
    record: &'static str,
    prefix: &str,
    signature: &str,
    at: i64,
) -> Delivery {
    let header = |name: &str| -> &'static str {
        match (prefix, name) {
            ("webhook", "id") => "webhook-id",
            ("webhook", "timestamp") => "webhook-timestamp",
            ("webhook", _) => "webhook-signature",
            (_, "id") => "svix-id",
            (_, "timestamp") => "svix-timestamp",
            _ => "svix-signature",
        }
    };
    Delivery {
        what,
        record,
        method: "POST",
        uri: format!("/webhooks/t/standard?delivery={record}"),
        headers: vec![
            (header("id"), SVIX_ID.to_string()),
            (header("timestamp"), SVIX_T.to_string()),
            (header("signature"), signature.to_string()),
        ],
        body: SVIX_BODY,
        at: Some(at),
    }
}

fn token(what: &'static str, record: &'static str, path: &str, value: &str) -> Delivery {
    Delivery {
        what,
        record,
        method: "POST",
        uri: format!("/webhooks/t/{path}?delivery={record}"),
        headers: vec![("X-Gitlab-Token", value.to_string())],
        body: "{}",
        at: None,
    }
}

impl Delivery {
    fn with_body(mut self, body: &'static str) -> Self {
        self.body = body;
        self
    }

    fn without_headers(mut self) -> Self {
        self.headers.clear();
        self
    }

    fn without_header(mut self, name: &str) -> Self {
        self.headers.retain(|(header, _)| *header != name);
        self
    }

    fn with_uri(mut self, uri: &str) -> Self {
        self.uri = uri.to_string();
        self
    }
}

/// Create the records the deliveries name, send each delivery, and list those
/// whose answer or record, read back after, is not the expected one.
async fn outcomes_that_differ(
    deliveries: Vec<Delivery>,
    expected_status: StatusCode,
    expected_record: &str,
) -> Vec<String> {
    let clock = Arc::new(ProviderClock(AtomicI64::new(Utc::now().timestamp())));
    let _clock = install_sim_context(clock.clone(), Arc::new(RealIdGen));
    let state = build_state();
    let records: std::collections::BTreeSet<_> =
        deliveries.iter().map(|delivery| delivery.record).collect();
    for record in records {
        create_delivery(&state, record).await;
    }
    let mut differ = Vec::new();
    for delivery in deliveries {
        if let Some(at) = delivery.at {
            clock.0.store(at, Ordering::SeqCst);
        }
        let (status, body) = send(&state, delivery.request()).await;
        let record = delivery_status(&state, delivery.record).await;
        let answered_right = if expected_status == StatusCode::OK {
            status == StatusCode::OK
                && serde_json::from_str::<Value>(&body).is_ok_and(|response| {
                    response["success"] == true && response["state"]["status"] == expected_record
                })
        } else {
            status == expected_status && body == "Webhook admission denied"
        };
        if !answered_right || record != expected_record {
            differ.push(format!(
                "{}: answered {status} {body:?}, record {} is {record}",
                delivery.what, delivery.record
            ));
        }
    }
    differ
}

#[tokio::test]
async fn a_request_its_provider_signed_moves_the_record_it_names() {
    let deliveries = vec![
        kernel("Temper's own signature", "1", "POST"),
        github("GitHub's published example", "github"),
        hmac("body HMAC in hex", "hmac-hex", HMAC_HEX),
        hmac("body HMAC in base64", "hmac-base64", HMAC_BASE64),
        hmac(
            "body HMAC in hex after sha256=",
            "hmac-prefixed",
            &format!("sha256={HMAC_HEX}"),
        ),
        stripe("Stripe's v1 signature", "stripe", stripe_signed(), STRIPE_T),
        standard(
            "Svix's published example in webhook- headers",
            "standard",
            "webhook",
            SVIX_SIGNATURE,
            SVIX_T,
        ),
        standard(
            "Svix's published example in svix- headers",
            "svix",
            "svix",
            SVIX_SIGNATURE,
            SVIX_T,
        ),
        standard(
            "a rotated secret's list of signatures, one of them good",
            "standard-rotated",
            "webhook",
            &format!("v1,AAAAbm90IHRoZSBvbmU= v1a,ignored {SVIX_SIGNATURE}"),
            SVIX_T,
        ),
        token("the shared token", "token", "token", GITLAB_TOKEN),
    ];
    let differ = outcomes_that_differ(deliveries, StatusCode::OK, "Received").await;
    assert!(
        differ.is_empty(),
        "signed requests must be answered 200 and move their record to Received; these were not: {differ:#?}"
    );
}

#[tokio::test]
async fn a_request_without_its_providers_signature_is_refused_and_moves_nothing() {
    let deliveries = vec![
        kernel("Temper's signature, for another record", "2", "POST").with_uri("/webhooks/t/p?x=2"),
        kernel("Temper's signature, sent with another method", "1", "PUT"),
        kernel("Temper's signature, over another body", "1", "POST").with_body(r#"{"a":2}"#),
        kernel("no signature header", "1", "POST").without_headers(),
        github("GitHub's signature, over another body", "github-body").with_body("Hello, World?"),
        github("no GitHub signature header", "github-none").without_headers(),
        hmac("a body HMAC cut short", "hmac-short", &HMAC_HEX[..62]),
        hmac("no body HMAC header", "hmac-none", HMAC_HEX).without_headers(),
        stripe(
            "Stripe's signature, over another body",
            "stripe-body",
            stripe_signed(),
            STRIPE_T,
        )
        .with_body("{}"),
        stripe(
            "Stripe's v1 value offered only as v0",
            "stripe-v0",
            format!("t={STRIPE_T},v0={STRIPE_V1}"),
            STRIPE_T,
        ),
        stripe(
            "Stripe's signature with its signed time changed",
            "stripe-moved",
            format!("t={},v1={STRIPE_V1}", STRIPE_T + 1),
            STRIPE_T,
        ),
        stripe(
            "no Stripe-Signature header",
            "stripe-none",
            String::new(),
            STRIPE_T,
        )
        .without_headers(),
        standard(
            "Svix's signature, over another body",
            "standard-body",
            "webhook",
            SVIX_SIGNATURE,
            SVIX_T,
        )
        .with_body("{}"),
        standard(
            "only an asymmetric v1a signature",
            "standard-v1a",
            "webhook",
            "v1a,cmF3",
            SVIX_T,
        ),
        standard(
            "no webhook-id header",
            "standard-no-id",
            "webhook",
            SVIX_SIGNATURE,
            SVIX_T,
        )
        .without_header("webhook-id"),
        token("a wrong token", "token-wrong", "token", "s3cret!"),
        token("no token header", "token-none", "token", GITLAB_TOKEN).without_headers(),
        token(
            "a webhook whose secret is not in the vault",
            "unresolved",
            "unresolved",
            "{secret:NOT_IN_THE_VAULT}",
        ),
    ];
    let differ = outcomes_that_differ(deliveries, StatusCode::UNAUTHORIZED, "Waiting").await;
    assert!(
        differ.is_empty(),
        "unsigned or missigned requests must be answered 401 and leave their record Waiting; these were not: {differ:#?}"
    );
}

#[tokio::test]
async fn a_signature_signed_more_than_five_minutes_from_now_is_refused() {
    let edge = TOLERANCE_SECONDS;
    let on_time = vec![
        stripe(
            "Stripe, signed five minutes ago",
            "stripe-old",
            stripe_signed(),
            STRIPE_T + edge,
        ),
        stripe(
            "Stripe, signed five minutes ahead",
            "stripe-ahead",
            stripe_signed(),
            STRIPE_T - edge,
        ),
        standard(
            "Svix, signed five minutes ago",
            "standard-old",
            "webhook",
            SVIX_SIGNATURE,
            SVIX_T + edge,
        ),
        standard(
            "Svix, signed five minutes ahead",
            "standard-ahead",
            "webhook",
            SVIX_SIGNATURE,
            SVIX_T - edge,
        ),
    ];
    let late = vec![
        stripe(
            "Stripe, replayed five minutes and a second later",
            "stripe-replayed",
            stripe_signed(),
            STRIPE_T + edge + 1,
        ),
        stripe(
            "Stripe, signed five minutes and a second ahead",
            "stripe-early",
            stripe_signed(),
            STRIPE_T - edge - 1,
        ),
        standard(
            "Svix, replayed five minutes and a second later",
            "standard-replayed",
            "webhook",
            SVIX_SIGNATURE,
            SVIX_T + edge + 1,
        ),
        standard(
            "Svix, signed five minutes and a second ahead",
            "standard-early",
            "webhook",
            SVIX_SIGNATURE,
            SVIX_T - edge - 1,
        ),
    ];
    let mut differ = outcomes_that_differ(on_time, StatusCode::OK, "Received").await;
    differ.extend(outcomes_that_differ(late, StatusCode::UNAUTHORIZED, "Waiting").await);
    assert!(
        differ.is_empty(),
        "signatures up to five minutes off must be admitted and later ones refused; these were not: {differ:#?}"
    );
}

/// HMAC-SHA256 of `body` under the body-HMAC secret, in hex: the signature
/// the published example above shows the receiver accepts.
fn body_hmac(body: &[u8]) -> String {
    use hmac::{Hmac, Mac};
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(HMAC_SECRET.as_bytes())
        .expect("HMAC takes a key of any length");
    mac.update(body);
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// An event that names its record at `body.data.object.reference`, padded
/// past 64 KiB, a size the receiver once refused with no reason given.
fn event(reference: Option<Value>) -> Vec<u8> {
    let mut object = json!({ "amount": 1999 });
    if let Some(reference) = reference {
        object["reference"] = reference;
    }
    serde_json::to_vec(&json!({
        "id": "evt_1",
        "data": {
            "object": object,
            "items": [{ "sku": "sku-1" }],
            "padding": "x".repeat(200 * 1024),
        }
    }))
    .expect("the event serializes")
}

fn event_post(body: Vec<u8>, signature: &str) -> Request<Body> {
    Request::post("/webhooks/t/event?source=dashboard")
        .header("x-signature", signature)
        .header("x-delivery", "delivery-7")
        .body(Body::from(body))
        .expect("the request builds")
}

#[tokio::test]
async fn a_signed_request_hands_its_body_query_and_headers_to_the_action() {
    let state = build_state();
    create_delivery(&state, "rec-1").await;
    create_delivery(&state, "42").await;

    let mut differ = Vec::new();
    for (reference, record) in [(json!("rec-1"), "rec-1"), (json!(42), "42")] {
        let body = event(Some(reference.clone()));
        let signature = body_hmac(&body);
        let (status, response) = send(&state, event_post(body, &signature)).await;
        if status != StatusCode::OK {
            differ.push(format!(
                "reference {reference}: answered {status} {response}"
            ));
        }
        let read = read_delivery(&state, record).await;
        let fields = &read["fields"];
        let actual = json!({
            "status": read["status"],
            "amount": fields["amount"],
            "event": fields["event"],
            "delivery": fields["delivery"],
            "source": fields["source"],
            "first_item": fields["first_item"],
        });
        let expected = json!({
            "status": "Received",
            "amount": 1999,
            "event": "evt_1",
            "delivery": "delivery-7",
            "source": "dashboard",
            "first_item": "sku-1",
        });
        if actual != expected {
            differ.push(format!(
                "reference {reference}: record {record} reads {actual}, not {expected}"
            ));
        }
    }
    assert!(
        differ.is_empty(),
        "a signed event must move the record it names, by text or by number, with its body, query and header values as the action's: {differ:#?}"
    );
}

#[tokio::test]
async fn a_request_the_receiver_cannot_read_is_refused_only_after_its_signature_is_checked() {
    let state = build_state();
    let no_reference = event(None);
    let not_json = b"not json".to_vec();
    let cases = [
        (
            "a signed event that names no record",
            event_post(no_reference.clone(), &body_hmac(&no_reference)),
            StatusCode::BAD_REQUEST,
            "Missing entity id: the request has nothing at 'body.data.object.reference'",
        ),
        (
            "a signed body that is not JSON",
            event_post(not_json.clone(), &body_hmac(&not_json)),
            StatusCode::BAD_REQUEST,
            "Webhook 'event' reads the body, which is not JSON",
        ),
        (
            "an unsigned body that is not JSON",
            event_post(not_json.clone(), &body_hmac(b"another body")),
            StatusCode::UNAUTHORIZED,
            "Webhook admission denied",
        ),
    ];
    let mut differ = Vec::new();
    for (what, request, expected_status, expected_body) in cases {
        let (status, body) = send(&state, request).await;
        if status != expected_status || !body.starts_with(expected_body) {
            differ.push(format!(
                "{what}: answered {status} {body:?}, not {expected_status} {expected_body:?}"
            ));
        }
    }
    assert!(
        differ.is_empty(),
        "unreadable requests must be refused, and an unsigned one before its body is read: {differ:#?}"
    );
}

/// An action on a record, over OData.
async fn act(state: &ServerState, id: &str, action: &str) -> StatusCode {
    let request = Request::post(format!("/tdata/Deliveries('{id}')/Temper.Hook.{action}"))
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .expect("the request builds");
    send(state, as_desk(request)).await.0
}

#[tokio::test]
async fn a_transition_is_posted_to_each_outgoing_webhook_that_matches_it() {
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // SAFETY: set before any other thread reads the environment; nextest runs
    // each test in its own process.
    unsafe { std::env::set_var("WEBHOOK_SIGNATURES_TOKEN", "token-from-the-environment") };
    let receiver = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&receiver)
        .await;
    let url = receiver.uri();
    let config = format!(
        r#"
[[webhook]]
name = "every"
url = "{url}/every"
payload_template = '{{"tenant":"${{tenant}}","type":"${{entity_type}}","id":"${{entity_id}}","action":"${{action}}","from":"${{from_status}}","to":"${{to_status}}"}}'

[webhook.headers]
Authorization = "Bearer ${{WEBHOOK_SIGNATURES_TOKEN}}"
X-Unset = "before-${{WEBHOOK_SIGNATURES_NOT_SET}}-after"

[[webhook]]
name = "signings"
url = "{url}/signings"
actions = ["Sign"]
payload_template = '${{entity_id}} ${{action}}'

[[webhook]]
name = "invoices"
url = "{url}/invoices"
entity_types = ["Invoice"]
payload_template = '${{entity_id}} ${{action}}'
"#
    );
    let dispatcher = WebhookDispatcher::from_toml(&config).expect("the webhooks parse");
    let state = build_state().with_webhook_dispatcher(Arc::new(dispatcher));

    create_delivery(&state, "d-1").await;
    let steps = [
        ("Receive", act(&state, "d-1", "Receive").await),
        ("Receive again", act(&state, "d-1", "Receive").await),
        ("Sign", act(&state, "d-1", "Sign").await),
    ];
    assert_eq!(
        steps.map(|(step, status)| (step, status.as_u16())),
        [("Receive", 200), ("Receive again", 409), ("Sign", 200)],
        "the actions are answered as the spec allows"
    );

    // Posts are sent in the background: wait for the two transitions, then a
    // moment more for any post that should not have been sent.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while receiver.received_requests().await.unwrap_or_default().len() < 3
        && tokio::time::Instant::now() < deadline
    {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    let mut every = Vec::new();
    let mut others = Vec::new();
    for request in receiver.received_requests().await.unwrap_or_default() {
        let body = String::from_utf8_lossy(&request.body).into_owned();
        let header = |name: &str| {
            request
                .headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_string()
        };
        match request.url.path() {
            "/every" => every.push((
                serde_json::from_str::<Value>(&body).unwrap_or(Value::String(body)),
                header("authorization"),
                header("x-unset"),
                header("content-type"),
            )),
            path => others.push(format!("{path} {body}")),
        }
    }
    let transition = |action: &str, from: &str, to: &str| json!({"tenant": TENANT, "type": "Delivery", "id": "d-1", "action": action, "from": from, "to": to});
    let headers = |payload: Value| {
        (
            payload,
            "Bearer token-from-the-environment".to_string(),
            "before--after".to_string(),
            "application/json".to_string(),
        )
    };
    every.sort_by_key(|(payload, ..)| payload["action"].as_str().map(str::to_string));
    assert_eq!(
        every,
        vec![
            headers(transition("Receive", "Waiting", "Received")),
            headers(transition("Sign", "Received", "Signed")),
        ],
        "the unfiltered webhook gets each transition, and not the refused action"
    );
    assert_eq!(
        others,
        vec!["/signings d-1 Sign".to_string()],
        "a filtered webhook gets only the actions and types it names"
    );
}
