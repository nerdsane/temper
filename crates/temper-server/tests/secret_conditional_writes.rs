//! Conditional writes for tenant secrets: `If-None-Match: *` creates only,
//! `If-Match: "<etag>"` replaces only an unchanged key, and every successful
//! write answers with the key's new `ETag`.
//!
//! Every case runs against Turso. The same cases run against Postgres when
//! `TEMPER_TEST_POSTGRES_URL` names a disposable database (ignored otherwise;
//! the Postgres rollback-migrations CI job runs them).

#[path = "secret_conditional_writes/support.rs"]
mod support;

use axum::Router;
use axum::http::StatusCode;
use support::{
    Backend, CONCURRENT_WRITERS, PutOutcome, WRITER, listed_etag, put, refused, written,
};

// ---------------------------------------------------------------------------
// Scenarios, each run against every store.
// ---------------------------------------------------------------------------

async fn create_only_refuses_an_existing_key(backend: Backend) {
    let app = backend.router();
    let first = put(&app, "signin", "first", WRITER, &[("if-none-match", "*")]).await;
    let etag = written(&first);
    assert_eq!(listed_etag(&app, "signin").await.as_deref(), Some(&*etag));

    let second = put(&app, "signin", "second", WRITER, &[("if-none-match", "*")]).await;
    refused(&second, &etag);
    assert_eq!(
        backend.stored_value("signin").await.as_deref(),
        Some("first")
    );
    assert_eq!(listed_etag(&app, "signin").await.as_deref(), Some(&*etag));
    backend.finish().await;
}

async fn replace_needs_the_current_etag(backend: Backend) {
    let app = backend.router();
    let missing = put(
        &app,
        "signin",
        "nobody",
        WRITER,
        &[("if-match", "\"nope\"")],
    )
    .await;
    refused(&missing, "nope");
    assert_eq!(backend.stored_value("signin").await, None);

    let first = written(&put(&app, "signin", "first", WRITER, &[]).await);
    let second = written(&put(&app, "signin", "second", WRITER, &[("if-match", &first)]).await);
    assert_ne!(first, second, "every write gets a new etag");
    assert_eq!(listed_etag(&app, "signin").await.as_deref(), Some(&*second));

    let stale = put(&app, "signin", "stale", WRITER, &[("if-match", &first)]).await;
    refused(&stale, &second);
    assert_eq!(
        backend.stored_value("signin").await.as_deref(),
        Some("second")
    );
    backend.finish().await;
}

async fn etag_is_not_derived_from_the_value(backend: Backend) {
    let app = backend.router();
    let a = written(&put(&app, "one", "same-value", WRITER, &[]).await);
    let b = written(&put(&app, "one", "same-value", WRITER, &[]).await);
    let c = written(&put(&app, "two", "same-value", WRITER, &[]).await);
    assert_ne!(a, b, "rewriting the same value changes the etag");
    assert_ne!(a, c, "two keys holding one value have different etags");
    assert_ne!(b, c, "two keys holding one value have different etags");
    backend.finish().await;
}

async fn unconditional_writes_are_unchanged(backend: Backend) {
    let app = backend.router();
    written(&put(&app, "plain", "first", WRITER, &[]).await);
    written(&put(&app, "plain", "second", WRITER, &[]).await);
    assert_eq!(
        backend.stored_value("plain").await.as_deref(),
        Some("second")
    );
    backend.finish().await;
}

async fn authorization_comes_before_the_condition(backend: Backend) {
    let app = backend.router();
    let etag = written(&put(&app, "signin", "first", WRITER, &[]).await);
    for headers in [[("if-match", "\"nope\"")], [("if-none-match", "*")]] {
        let denied = put(&app, "signin", "intruder", "intruder", &headers).await;
        assert_eq!(denied.status, StatusCode::FORBIDDEN, "{denied:?}");
        assert_eq!(denied.etag, None);
        assert!(!denied.body.contains(etag.trim_matches('"')));
    }
    assert_eq!(
        backend.stored_value("signin").await.as_deref(),
        Some("first")
    );
    backend.finish().await;
}

async fn unsupported_conditions_leave_the_secret_unchanged(backend: Backend) {
    let app = backend.router();
    let etag = written(&put(&app, "signin", "first", WRITER, &[]).await);
    let weak = format!("W/{etag}");
    let list = format!("{etag}, \"other\"");
    let cases: &[(&str, &[(&str, &str)])] = &[
        ("weak tag", &[("if-match", &weak)]),
        ("tag list", &[("if-match", &list)]),
        ("match wildcard", &[("if-match", "*")]),
        ("non-wildcard none-match", &[("if-none-match", &etag)]),
        (
            "duplicate match",
            &[("if-match", &etag), ("if-match", &etag)],
        ),
        (
            "duplicate none-match",
            &[("if-none-match", "*"), ("if-none-match", "*")],
        ),
        (
            "both headers",
            &[("if-match", &etag), ("if-none-match", "*")],
        ),
    ];
    for (name, headers) in cases {
        let outcome = put(&app, "signin", "overwrite", WRITER, headers).await;
        assert_eq!(
            outcome.status,
            StatusCode::BAD_REQUEST,
            "{name}: {outcome:?}"
        );
        assert_eq!(outcome.etag, None, "{name}");
        assert_eq!(
            backend.stored_value("signin").await.as_deref(),
            Some("first"),
            "{name}"
        );
        assert_eq!(
            listed_etag(&app, "signin").await.as_deref(),
            Some(etag.as_str()),
            "{name}"
        );
    }
    backend.finish().await;
}

/// Fires `CONCURRENT_WRITERS` PUTs at once, each with a distinct value and the
/// same condition, and returns what each one got.
async fn race(app: &Router, key: &str, headers: Vec<(String, String)>) -> Vec<PutOutcome> {
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(CONCURRENT_WRITERS));
    let mut writers = Vec::new();
    for writer in 0..CONCURRENT_WRITERS {
        let app = app.clone();
        let key = key.to_string();
        let headers = headers.clone();
        let barrier = barrier.clone();
        writers.push(tokio::spawn(async move {
            let headers: Vec<(&str, &str)> = headers
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str()))
                .collect();
            barrier.wait().await;
            put(&app, &key, &format!("writer-{writer}"), WRITER, &headers).await
        }));
    }
    let mut outcomes = Vec::new();
    for writer in writers {
        outcomes.push(writer.await.expect("writer task"));
    }
    outcomes
}

/// Exactly one writer wins, every other writer gets 412, and the store holds
/// the winner's value under the winner's etag.
async fn assert_one_winner(backend: &Backend, app: &Router, key: &str, outcomes: &[PutOutcome]) {
    let winners: Vec<&PutOutcome> = outcomes
        .iter()
        .filter(|outcome| outcome.status.is_success())
        .collect();
    assert_eq!(winners.len(), 1, "exactly one writer wins: {outcomes:?}");
    let winner = winners[0];
    let etag = written(winner);
    for loser in outcomes
        .iter()
        .filter(|outcome| !outcome.status.is_success())
    {
        refused(loser, &etag);
    }
    assert_eq!(listed_etag(app, key).await.as_deref(), Some(&*etag));
    assert_eq!(
        backend.stored_value(key).await.as_deref(),
        Some(winner.value.as_str())
    );
}

async fn concurrent_creators_have_one_winner(backend: Backend) {
    let app = backend.router();
    let outcomes = race(&app, "signin", vec![("if-none-match".into(), "*".into())]).await;
    assert_one_winner(&backend, &app, "signin", &outcomes).await;
    backend.finish().await;
}

async fn concurrent_replacers_have_one_winner(backend: Backend) {
    let app = backend.router();
    let etag = written(&put(&app, "signin", "older", WRITER, &[]).await);
    let outcomes = race(&app, "signin", vec![("if-match".into(), etag.clone())]).await;
    assert_one_winner(&backend, &app, "signin", &outcomes).await;
    let after = listed_etag(&app, "signin")
        .await
        .expect("etag after the race");
    assert_ne!(after, etag, "the winning write replaced the etag");
    backend.finish().await;
}

// ---------------------------------------------------------------------------
// Turso
// ---------------------------------------------------------------------------

#[tokio::test]
async fn turso_create_only_refuses_an_existing_key() {
    create_only_refuses_an_existing_key(Backend::turso("cas-create").await).await;
}

#[tokio::test]
async fn turso_replace_needs_the_current_etag() {
    replace_needs_the_current_etag(Backend::turso("cas-replace").await).await;
}

#[tokio::test]
async fn turso_etag_is_not_derived_from_the_value() {
    etag_is_not_derived_from_the_value(Backend::turso("cas-opaque").await).await;
}

#[tokio::test]
async fn turso_unconditional_writes_are_unchanged() {
    unconditional_writes_are_unchanged(Backend::turso("cas-plain").await).await;
}

#[tokio::test]
async fn turso_authorization_comes_before_the_condition() {
    authorization_comes_before_the_condition(Backend::turso("cas-authz").await).await;
}

#[tokio::test]
async fn turso_unsupported_conditions_leave_the_secret_unchanged() {
    unsupported_conditions_leave_the_secret_unchanged(Backend::turso("cas-invalid").await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn turso_concurrent_creators_have_one_winner() {
    concurrent_creators_have_one_winner(Backend::turso("cas-race-create").await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn turso_concurrent_replacers_have_one_winner() {
    concurrent_replacers_have_one_winner(Backend::turso("cas-race-replace").await).await;
}

// ---------------------------------------------------------------------------
// Postgres
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires TEMPER_TEST_POSTGRES_URL pointing to disposable PostgreSQL"]
async fn postgres_create_only_refuses_an_existing_key() {
    create_only_refuses_an_existing_key(Backend::postgres("pg-cas-create").await).await;
}

#[tokio::test]
#[ignore = "requires TEMPER_TEST_POSTGRES_URL pointing to disposable PostgreSQL"]
async fn postgres_replace_needs_the_current_etag() {
    replace_needs_the_current_etag(Backend::postgres("pg-cas-replace").await).await;
}

#[tokio::test]
#[ignore = "requires TEMPER_TEST_POSTGRES_URL pointing to disposable PostgreSQL"]
async fn postgres_etag_is_not_derived_from_the_value() {
    etag_is_not_derived_from_the_value(Backend::postgres("pg-cas-opaque").await).await;
}

#[tokio::test]
#[ignore = "requires TEMPER_TEST_POSTGRES_URL pointing to disposable PostgreSQL"]
async fn postgres_unconditional_writes_are_unchanged() {
    unconditional_writes_are_unchanged(Backend::postgres("pg-cas-plain").await).await;
}

#[tokio::test]
#[ignore = "requires TEMPER_TEST_POSTGRES_URL pointing to disposable PostgreSQL"]
async fn postgres_authorization_comes_before_the_condition() {
    authorization_comes_before_the_condition(Backend::postgres("pg-cas-authz").await).await;
}

#[tokio::test]
#[ignore = "requires TEMPER_TEST_POSTGRES_URL pointing to disposable PostgreSQL"]
async fn postgres_unsupported_conditions_leave_the_secret_unchanged() {
    unsupported_conditions_leave_the_secret_unchanged(Backend::postgres("pg-cas-invalid").await)
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires TEMPER_TEST_POSTGRES_URL pointing to disposable PostgreSQL"]
async fn postgres_concurrent_creators_have_one_winner() {
    concurrent_creators_have_one_winner(Backend::postgres("pg-cas-race-create").await).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires TEMPER_TEST_POSTGRES_URL pointing to disposable PostgreSQL"]
async fn postgres_concurrent_replacers_have_one_winner() {
    concurrent_replacers_have_one_winner(Backend::postgres("pg-cas-race-replace").await).await;
}
