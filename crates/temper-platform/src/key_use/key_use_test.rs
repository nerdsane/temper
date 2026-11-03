use super::*;
use crate::bearer_auth::bearer_auth_check;
use crate::state::PlatformState;
use axum::Router;
use axum::body::Body;
use axum::http::StatusCode;
use axum::middleware;
use axum::routing::get;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, SubscriberExt};

const CONFIGURED_KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const OLDER_KEY: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
const UNKNOWN_KEY: &str = "aaaabbbbccccddddeeeeffff0000111122223333444455556666777788889999";
const CLIENT_ADDRESS: &str = "203.0.113.9";
const FIELDS: [&str; 10] = [
    "agent", "caller", "country", "evt", "key", "key_name", "message", "method", "outcome", "route",
];

/// Every `key_use` event logged while it is the thread's subscriber, as its
/// fields.
#[derive(Clone, Default)]
struct Events(Arc<Mutex<Vec<BTreeMap<String, String>>>>);

#[derive(Default)]
struct Fields(BTreeMap<String, String>);

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_string(), value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.0
            .insert(field.name().to_string(), format!("{value:?}"));
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Events {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        if fields.0.get("evt").map(String::as_str) == Some("key_use") {
            self.0.lock().unwrap().push(fields.0);
        }
    }
}

impl Events {
    fn taken(&self) -> Vec<BTreeMap<String, String>> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

async fn ok_handler() -> &'static str {
    "ok"
}

/// A tenant whose operator credential is `issued`, on a deployment whose
/// `TEMPER_API_KEY` is `configured`.
async fn edge(issued: &str, configured: &str) -> Router {
    let mut state = PlatformState::new(None);
    state.api_token = Some(configured.to_string());
    crate::bootstrap::bootstrap_agent_specs(&state, "default", false, &BTreeMap::new());
    crate::bootstrap::bootstrap_operator_credential(&state, issued, "default")
        .await
        .expect("operator credential bootstrap should succeed");
    Router::new()
        .route("/tdata/{*path}", get(ok_handler))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            bearer_auth_check,
        ))
        .with_state(state)
}

async fn send(router: &Router, bearer: &str) -> StatusCode {
    router
        .clone()
        .oneshot(
            Request::get("/tdata/Repositories('rp-1234')")
                .header("authorization", format!("Bearer {bearer}"))
                .header("x-tenant-id", "default")
                .header("x-forwarded-for", format!("{CLIENT_ADDRESS}, 10.0.0.1"))
                .header("user-agent", "node")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

/// No value of the event holds the key, the start of it, its hash or the
/// caller's address.
fn assert_holds_no_secret(event: &BTreeMap<String, String>, key: &str) {
    let key_hash = temper_server::identity::hash_token(key);
    for value in event.values() {
        for secret in [
            key,
            &key[..8],
            key_hash.as_str(),
            &key_hash[..8],
            CLIENT_ADDRESS,
        ] {
            assert!(
                !value.contains(secret),
                "event value {value:?} holds a secret"
            );
        }
    }
}

#[tokio::test]
async fn the_configured_operator_key_logs_one_accepted_event() {
    let events = Events::default();
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(events.clone()));
    let router = edge(CONFIGURED_KEY, CONFIGURED_KEY).await;
    events.taken();

    assert_eq!(send(&router, CONFIGURED_KEY).await, StatusCode::OK);

    let logged = events.taken();
    assert_eq!(logged.len(), 1, "one event per request: {logged:?}");
    let event = &logged[0];
    assert_eq!(event.keys().map(String::as_str).collect::<Vec<_>>(), FIELDS);
    assert_eq!(event["outcome"], "accepted");
    assert_eq!(event["key"], "operator");
    assert_eq!(event["key_name"], OPERATOR);
    assert_eq!(event["route"], "/tdata/{*path}");
    assert_eq!(event["method"], "GET");
    assert_eq!(event["country"], "");
    assert_eq!(event["agent"], "node");
    assert_holds_no_secret(event, CONFIGURED_KEY);
}

#[tokio::test]
async fn an_older_operator_key_is_named_apart_from_the_configured_one() {
    let events = Events::default();
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(events.clone()));
    let router = edge(OLDER_KEY, CONFIGURED_KEY).await;
    events.taken();

    assert_eq!(send(&router, OLDER_KEY).await, StatusCode::OK);

    let logged = events.taken();
    assert_eq!(logged.len(), 1, "{logged:?}");
    assert_eq!(logged[0]["outcome"], "accepted");
    assert_eq!(logged[0]["key_name"], OTHER_OPERATOR);
    assert_holds_no_secret(&logged[0], OLDER_KEY);
}

#[tokio::test]
async fn an_operator_shaped_key_that_does_not_match_logs_one_refused_event() {
    let events = Events::default();
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(events.clone()));
    let router = edge(CONFIGURED_KEY, CONFIGURED_KEY).await;
    events.taken();

    assert_eq!(send(&router, UNKNOWN_KEY).await, StatusCode::UNAUTHORIZED);

    let logged = events.taken();
    assert_eq!(logged.len(), 1, "{logged:?}");
    let event = &logged[0];
    assert_eq!(event.keys().map(String::as_str).collect::<Vec<_>>(), FIELDS);
    assert_eq!(event["outcome"], "refused");
    assert_eq!(event["key_name"], OPERATOR);
    assert_eq!(event["route"], "/tdata/{*path}");
    assert_holds_no_secret(event, UNKNOWN_KEY);
}

#[tokio::test]
async fn a_credential_not_shaped_like_an_operator_key_logs_nothing() {
    let events = Events::default();
    let _guard =
        tracing::subscriber::set_default(tracing_subscriber::registry().with(events.clone()));
    let router = edge(CONFIGURED_KEY, CONFIGURED_KEY).await;
    events.taken();

    assert_eq!(
        send(&router, "tmpr_not-a-real-key").await,
        StatusCode::UNAUTHORIZED
    );

    assert!(events.taken().is_empty());
}

fn request_from(peer: [u8; 4], headers: &[(&str, &str)]) -> Request<Body> {
    let mut request = Request::get("/paw/setup/status");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let mut request = request.body(Body::empty()).unwrap();
    request
        .extensions_mut()
        .insert(ConnectInfo(SocketAddr::from((peer, 443))));
    request
}

fn expected_caller(salt: &str, address: &str) -> String {
    let digest = Sha256::digest(format!("{salt}{address}").as_bytes());
    digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()[..12]
        .to_string()
}

#[test]
fn the_caller_is_the_salted_hash_of_the_address() {
    let salted = |request: &Request<Body>| {
        KeyUse::from_request(request, Outcome::Refused, OPERATOR, Some("pepper")).caller
    };

    let from_peer = request_from([198, 51, 100, 7], &[]);
    assert_eq!(
        salted(&from_peer),
        expected_caller("pepper", "198.51.100.7")
    );
    assert_ne!(
        salted(&from_peer),
        expected_caller("other-salt", "198.51.100.7")
    );

    let forwarded = request_from(
        [10, 0, 0, 1],
        &[("x-forwarded-for", "203.0.113.9, 10.0.0.1")],
    );
    assert_eq!(
        salted(&forwarded),
        expected_caller("pepper", CLIENT_ADDRESS)
    );

    let behind_railway = request_from(
        [10, 0, 0, 1],
        &[
            ("x-real-ip", "192.0.2.4"),
            ("x-forwarded-for", "203.0.113.9"),
        ],
    );
    assert_eq!(
        salted(&behind_railway),
        expected_caller("pepper", "192.0.2.4")
    );

    let not_an_address = request_from([198, 51, 100, 7], &[("x-forwarded-for", "unknown")]);
    assert_eq!(
        salted(&not_an_address),
        expected_caller("pepper", "198.51.100.7")
    );

    let unsalted = KeyUse::from_request(&from_peer, Outcome::Accepted, OPERATOR, None);
    assert_eq!(unsalted.caller, "unsalted");
    assert_eq!(unsalted.route, "unmatched");
    assert_eq!(unsalted.at_endpoint(Some("/repo.git")).route, "/repo.git");
}

#[test]
fn only_an_operator_shaped_credential_counts_as_an_attempt() {
    assert!(looks_like_operator_key(CONFIGURED_KEY));
    assert!(looks_like_operator_key(
        &CONFIGURED_KEY.to_ascii_uppercase()
    ));
    assert!(!looks_like_operator_key(&CONFIGURED_KEY[..63]));
    assert!(!looks_like_operator_key("tmpr_0123"));
    assert!(!looks_like_operator_key("eyJhbGciOiJFUzI1NiJ9.e30.sig"));
}

#[test]
fn user_agents_fall_into_a_few_families() {
    for (user_agent, family) in [
        ("node", "node"),
        ("undici", "node"),
        (
            "node-fetch/1.0 (+https://github.com/bitinn/node-fetch)",
            "node",
        ),
        ("axios/1.7.2", "node"),
        ("curl/8.7.1", "curl"),
        ("python-requests/2.32.3", "python"),
        ("Python/3.12 aiohttp/3.9.5", "python"),
        ("python-httpx/0.27.0", "python"),
        (
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15",
            "browser",
        ),
        ("temper-mcp/0.1", "other"),
        ("", "other"),
    ] {
        assert_eq!(agent_family(user_agent), family, "{user_agent:?}");
    }
}
