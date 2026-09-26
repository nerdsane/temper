use super::*;
use crate::{ProductionWasmHost, WasmHost};
use std::net::SocketAddr;

fn secrets(key: &str, value: &str) -> BTreeMap<String, String> {
    BTreeMap::from([(key.to_owned(), value.to_owned())])
}

#[test]
fn repeated_config_reuses_actual_clients_without_sharing_secrets() {
    let cache = HttpClientCache::default();
    let first = cache.get(&secrets("api_key", "one"), Duration::from_secs(30));
    let second = cache.get(&secrets("api_key", "two"), Duration::from_secs(30));
    assert!(
        Arc::ptr_eq(&first, &second),
        "must reuse clients, not just the map slot"
    );
    assert_eq!(cache.len(), 1);

    let first_host = ProductionWasmHost::with_client_cache(
        secrets("api_key", "one"),
        Duration::from_secs(30),
        &cache,
    );
    let second_host = ProductionWasmHost::with_client_cache(
        secrets("api_key", "two"),
        Duration::from_secs(30),
        &cache,
    );
    assert_eq!(first_host.get_secret("api_key").unwrap(), "one");
    assert_eq!(second_host.get_secret("api_key").unwrap(), "two");
}

#[test]
fn timeout_and_ca_rotation_partition_the_cache() {
    let cache = HttpClientCache::default();
    let plain = cache.get(&BTreeMap::new(), Duration::from_secs(30));
    let timeout = cache.get(&BTreeMap::new(), Duration::from_secs(60));
    // Invalid certificates are ignored by the unchanged client builder, but
    // even these values must partition the key. No PEM may leak into Debug.
    let ca_one = secrets("ca_cert:private", "private-test-root-one");
    let ca_two = secrets("ca_cert:private", "private-test-root-two");
    let first = cache.get(&ca_one, Duration::from_secs(30));
    let rotated = cache.get(&ca_two, Duration::from_secs(30));
    assert!(!Arc::ptr_eq(&plain, &timeout));
    assert!(!Arc::ptr_eq(&plain, &first));
    assert!(!Arc::ptr_eq(&first, &rotated));
    assert!(Arc::ptr_eq(
        &first,
        &cache.get(&ca_one, Duration::from_secs(30))
    ));
    assert_eq!(cache.len(), 4);
    assert!(!format!("{cache:?}").contains("private-test-root"));
}

#[test]
fn eviction_keeps_recently_used_clients_even_for_the_smallest_key() {
    let cache = HttpClientCache::with_budget(2);
    let secrets = BTreeMap::new();
    let hot = cache.get(&secrets, Duration::from_secs(1));
    let cold = cache.get(&secrets, Duration::from_secs(2));
    assert!(Arc::ptr_eq(
        &hot,
        &cache.get(&secrets, Duration::from_secs(1))
    ));
    let _third = cache.get(&secrets, Duration::from_secs(3));
    assert_eq!(cache.len(), 2);
    assert!(Arc::ptr_eq(
        &hot,
        &cache.get(&secrets, Duration::from_secs(1))
    ));
    assert!(!Arc::ptr_eq(
        &cold,
        &cache.get(&secrets, Duration::from_secs(2))
    ));
    assert_eq!(cache.len(), 2);
}

#[test]
fn concurrent_misses_initialize_one_retained_client_pair() {
    let cache = HttpClientCache::default();
    let start = std::sync::Barrier::new(8);
    let clients = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                scope.spawn(|| {
                    start.wait();
                    cache.get(&BTreeMap::new(), Duration::from_secs(30))
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert!(
        clients
            .iter()
            .all(|client| Arc::ptr_eq(&clients[0], client))
    );
    assert_eq!(cache.len(), 1);
}

#[test]
fn engine_handles_do_not_share_http_clients_even_when_they_share_compilation() {
    let first = crate::WasmEngine::new().unwrap();
    let second = crate::WasmEngine::new().unwrap();
    let a = first
        .http_clients()
        .get(&BTreeMap::new(), Duration::from_secs(30));
    let b = second
        .http_clients()
        .get(&BTreeMap::new(), Duration::from_secs(30));
    assert!(!Arc::ptr_eq(&a, &b));
}

#[tokio::test]
async fn cached_hosts_reuse_real_connections_and_keep_internal_redirects_disabled() {
    use axum::{Router, extract::ConnectInfo, http::HeaderMap, response::Redirect, routing::get};

    async fn echo(ConnectInfo(peer): ConnectInfo<SocketAddr>, headers: HeaderMap) -> String {
        let identity = headers
            .get("x-test-identity")
            .map_or("", |value| value.to_str().unwrap());
        format!("{peer}|{identity}")
    }
    async fn credentials(ConnectInfo(peer): ConnectInfo<SocketAddr>, headers: HeaderMap) -> String {
        format!(
            "{peer}|{}|{}",
            headers["authorization"].to_str().unwrap(),
            headers["x-tenant-id"].to_str().unwrap(),
        )
    }
    let app = Router::new()
        .route("/echo", get(echo))
        .route("/internal", get(credentials))
        .route("/redirect", get(|| async { Redirect::temporary("/echo") }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    let cache = HttpClientCache::default();
    let url = format!("http://{address}/echo");
    let mut peers = Vec::new();
    for identity in ["one", "two"] {
        let host = ProductionWasmHost::with_client_cache(
            secrets("api_key", identity),
            Duration::from_secs(5),
            &cache,
        );
        let (status, body) = host
            .http_call(
                "GET",
                &url,
                &[("x-test-identity".into(), identity.into())],
                "",
            )
            .await
            .unwrap();
        assert_eq!(status, 200);
        let (peer, observed_identity) = body.split_once('|').unwrap();
        assert_eq!(observed_identity, identity);
        peers.push(peer.to_owned());
    }
    assert_eq!(
        peers[0], peers[1],
        "hosts should use the same TCP connection"
    );

    let clients = cache.get(&BTreeMap::new(), Duration::from_secs(5));
    let redirected = clients
        .client
        .get(format!("http://{address}/redirect"))
        .send()
        .await
        .unwrap();
    assert_eq!(redirected.status(), 200);
    let _ = redirected.bytes().await.unwrap();
    let internal = clients
        .internal_client
        .get(format!("http://{address}/redirect"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        internal.status(),
        307,
        "internal capabilities must not follow redirects"
    );
    let _ = internal.bytes().await.unwrap();
    let first = clients
        .internal_client
        .get(&url)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let second = cache
        .get(&BTreeMap::new(), Duration::from_secs(5))
        .internal_client
        .get(&url)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(first, second, "internal clients must also reuse their pool");
    assert_ne!(
        first.split_once('|').unwrap().0,
        peers[0],
        "internal and public pools stay separate"
    );
    let mut internal_peers = Vec::new();
    for identity in ["one", "two"] {
        let host =
            ProductionWasmHost::with_client_cache(BTreeMap::new(), Duration::from_secs(5), &cache)
                .with_internal_api_base_url(Some(format!("http://{address}")))
                .with_internal_capability_issuer(Arc::new(move |_, _| {
                    crate::InternalHttpCapability::new(
                        format!("capability-{identity}"),
                        format!("tenant-{identity}"),
                    )
                }));
        let (status, body) = host
            .http_call(
                "GET",
                &format!("http://{address}/internal"),
                &[
                    ("Authorization".into(), "Bearer forged".into()),
                    ("X-Tenant-Id".into(), "forged-tenant".into()),
                ],
                "",
            )
            .await
            .unwrap();
        assert_eq!(status, 200);
        let parts: Vec<_> = body.split('|').collect();
        assert_eq!(parts[1], format!("Bearer capability-{identity}"));
        assert_eq!(parts[2], format!("tenant-{identity}"));
        internal_peers.push(parts[0].to_owned());
    }
    assert_eq!(internal_peers[0], internal_peers[1]);
    server.abort();
    let _ = server.await;
}

#[test]
fn attaching_shared_streams_preserves_cached_clients_and_registry_identity() {
    let cache = HttpClientCache::default();
    let streams = Arc::new(crate::http_stream::HttpStreamRegistry::new());
    let host =
        ProductionWasmHost::with_client_cache(BTreeMap::new(), Duration::from_secs(30), &cache)
            .with_http_streams(Arc::clone(&streams));
    assert!(Arc::ptr_eq(&streams, &host.http_streams()));
    assert_eq!(cache.len(), 1);
}
