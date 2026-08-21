use super::*;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use temper_authz::SecurityContext;

fn test_store(capacity: usize) -> (InternalInvocationCredentialStore, Arc<AtomicI64>) {
    let millis = Arc::new(AtomicI64::new(1_700_000_000_000));
    let now_millis = Arc::clone(&millis);
    let counter = Arc::new(AtomicU64::new(1));
    let token_counter = Arc::clone(&counter);
    let store = InternalInvocationCredentialStore::with_sources(
        capacity,
        Duration::from_secs(30),
        Arc::new(move || {
            DateTime::from_timestamp_millis(now_millis.load(Ordering::SeqCst))
                .expect("test timestamp must be valid")
        }),
        Arc::new(move || {
            let value = token_counter.fetch_add(1, Ordering::SeqCst);
            let mut bytes = [0_u8; 32];
            bytes[..8].copy_from_slice(&value.to_be_bytes());
            bytes
        }),
    );
    (store, millis)
}

fn context(tenant: &str, principal: &str) -> AuthenticatedRequestContext {
    AuthenticatedRequestContext::new(
        TenantId::new(tenant),
        SecurityContext::from_resolved_identity(principal, "worker", None),
    )
}

fn request(method: Method, target: &str) -> (Method, Uri) {
    (method, target.parse().expect("test URI must parse"))
}

#[test]
fn valid_credential_returns_exact_context_once() {
    let (store, _) = test_store(8);
    let mut security_context =
        SecurityContext::from_resolved_identity("agent-1", "worker", Some("session-1"));
    security_context.principal.role = Some("planner".to_string());
    security_context
        .context_attrs
        .insert("approvalLimit".to_string(), serde_json::json!(1_000));
    let token = store
        .issue_for_url(
            AuthenticatedRequestContext::new(TenantId::new("tenant-a"), security_context),
            "POST",
            "http://127.0.0.1:3000/tdata/Orders?mode=full",
        )
        .expect("credential should issue");
    let (method, uri) = request(Method::POST, "/tdata/Orders?mode=full");

    let resolved = store
        .consume_for_request(&token, &TenantId::new("tenant-a"), &method, &uri)
        .expect("credential should resolve");
    assert_eq!(resolved.tenant().as_str(), "tenant-a");
    assert_eq!(resolved.security_context().principal.id, "agent-1");
    assert_eq!(
        resolved.security_context().principal.role.as_deref(),
        Some("planner")
    );
    assert_eq!(
        resolved
            .security_context()
            .context_attrs
            .get("approvalLimit"),
        Some(&serde_json::json!(1_000))
    );
    assert_eq!(store.len(), Ok(0));
    assert!(matches!(
        store.consume_for_request(&token, &TenantId::new("tenant-a"), &method, &uri),
        Err(InternalInvocationCredentialError::InvalidCredential)
    ));
}

#[test]
fn wrong_tenant_method_and_path_each_fail_and_consume() {
    let (store, _) = test_store(8);
    let cases = [
        ("tenant-b", Method::GET, "/tdata/Orders"),
        ("tenant-a", Method::POST, "/tdata/Orders"),
        ("tenant-a", Method::GET, "/tdata/Other"),
        ("tenant-a", Method::GET, "/tdata/Orders?extra=1"),
    ];

    for (tenant, method, target) in cases {
        let token = store
            .issue_for_url(
                context("tenant-a", "agent-1"),
                "GET",
                "http://127.0.0.1:3000/tdata/Orders",
            )
            .expect("credential should issue");
        let uri = target.parse().expect("test URI must parse");
        assert!(matches!(
            store.consume_for_request(&token, &TenantId::new(tenant), &method, &uri),
            Err(InternalInvocationCredentialError::BindingMismatch)
        ));
        assert!(matches!(
            store.consume_for_request(
                &token,
                &TenantId::new("tenant-a"),
                &Method::GET,
                &"/tdata/Orders".parse().expect("test URI must parse"),
            ),
            Err(InternalInvocationCredentialError::InvalidCredential)
        ));
    }
}

#[test]
fn expired_credential_fails_closed() {
    let (store, millis) = test_store(8);
    let token = store
        .issue_for_url(context("tenant-a", "agent-1"), "GET", "http://local/tdata")
        .expect("credential should issue");
    millis.fetch_add(30_000, Ordering::SeqCst);
    assert!(matches!(
        store.consume_for_request(
            &token,
            &TenantId::new("tenant-a"),
            &Method::GET,
            &"/tdata".parse().expect("test URI must parse"),
        ),
        Err(InternalInvocationCredentialError::InvalidCredential)
    ));
}

#[test]
fn system_context_is_refused_at_issuance_and_consumption() {
    let (store, _) = test_store(8);
    let system =
        AuthenticatedRequestContext::new(TenantId::new("tenant-a"), SecurityContext::system());
    assert!(matches!(
        store.issue_for_url(system.clone(), "GET", "http://local/tdata/Orders"),
        Err(InternalInvocationCredentialError::SystemContextNotDelegable)
    ));

    // Defense in depth: even a legacy/injected entry cannot reconstitute
    // System authority at the bearer edge.
    let token = encode_token([42_u8; 32]);
    let digest = credential_digest(&token);
    let now = (store.now)();
    store
        .state
        .lock()
        .expect("test store lock must be available")
        .entries
        .insert(
            digest,
            CredentialEntry {
                context: system,
                tenant: TenantId::new("tenant-a"),
                method: "GET".to_string(),
                target: "/tdata/Orders".to_string(),
                expires_at: now + chrono::Duration::seconds(30),
                issue_sequence: 0,
            },
        );
    assert!(matches!(
        store.consume_for_request(
            &token,
            &TenantId::new("tenant-a"),
            &Method::GET,
            &"/tdata/Orders".parse().expect("test URI must parse"),
        ),
        Err(InternalInvocationCredentialError::SystemContextNotDelegable)
    ));
    assert_eq!(store.len(), Ok(0));
}

#[test]
fn capacity_evicts_oldest_credential_deterministically() {
    let (store, _) = test_store(2);
    let first = store
        .issue_for_url(context("tenant-a", "one"), "GET", "http://local/one")
        .expect("first credential should issue");
    let second = store
        .issue_for_url(context("tenant-a", "two"), "GET", "http://local/two")
        .expect("second credential should issue");
    let third = store
        .issue_for_url(context("tenant-a", "three"), "GET", "http://local/three")
        .expect("third credential should issue");
    assert_eq!(store.len(), Ok(2));

    assert!(matches!(
        store.consume_for_request(
            &first,
            &TenantId::new("tenant-a"),
            &Method::GET,
            &"/one".parse().expect("test URI must parse"),
        ),
        Err(InternalInvocationCredentialError::InvalidCredential)
    ));
    for (token, target, principal) in [(second, "/two", "two"), (third, "/three", "three")] {
        let resolved = store
            .consume_for_request(
                &token,
                &TenantId::new("tenant-a"),
                &Method::GET,
                &target.parse().expect("test URI must parse"),
            )
            .expect("retained credential should resolve");
        assert_eq!(resolved.security_context().principal.id, principal);
    }
}

#[test]
fn full_store_never_evicts_another_tenants_credentials() {
    let (store, _) = test_store(2);
    let first = store
        .issue_for_url(context("tenant-a", "one"), "GET", "http://local/one")
        .expect("first credential should issue");
    let second = store
        .issue_for_url(context("tenant-a", "two"), "GET", "http://local/two")
        .expect("second credential should issue");

    assert_eq!(
        store.issue_for_url(context("tenant-b", "other"), "GET", "http://local/other"),
        Err(InternalInvocationCredentialError::CapacityExhausted)
    );
    for (token, target, principal) in [(first, "/one", "one"), (second, "/two", "two")] {
        let resolved = store
            .consume_for_request(
                &token,
                &TenantId::new("tenant-a"),
                &Method::GET,
                &target.parse().expect("test URI must parse"),
            )
            .expect("another tenant must not evict this credential");
        assert_eq!(resolved.security_context().principal.id, principal);
    }
}

#[test]
fn canonical_target_uses_only_normalized_path_and_exact_query() {
    assert_eq!(
        canonical_request_target_from_url("http://LOCAL:80/a/../b?q=1%202&x=")
            .expect("URL should canonicalize"),
        "/b?q=1%202&x="
    );
}
