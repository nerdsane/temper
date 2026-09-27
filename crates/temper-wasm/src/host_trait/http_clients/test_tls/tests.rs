use super::super::{HttpClientCache, HttpClients};
#[cfg(target_os = "linux")]
use openssl::{
    asn1::Asn1Time,
    bn::BigNum,
    hash::MessageDigest,
    pkey::PKey,
    rsa::Rsa,
    x509::{
        X509, X509NameBuilder,
        extension::{BasicConstraints, SubjectAlternativeName},
    },
};
use std::{collections::BTreeMap, time::Duration};
#[cfg(target_os = "linux")]
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[cfg(target_os = "linux")]
fn tls_fixture() -> (String, native_tls::Identity) {
    let root_key = PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap();
    let mut root_name = X509NameBuilder::new().unwrap();
    root_name
        .append_entry_by_text("CN", "temper test-only CA")
        .unwrap();
    let root_name = root_name.build();
    let mut root = X509::builder().unwrap();
    root.set_version(2).unwrap();
    root.set_serial_number(&BigNum::from_u32(1).unwrap().to_asn1_integer().unwrap())
        .unwrap();
    root.set_subject_name(&root_name).unwrap();
    root.set_issuer_name(&root_name).unwrap();
    root.set_pubkey(&root_key).unwrap();
    root.set_not_before(&Asn1Time::days_from_now(0).unwrap())
        .unwrap();
    root.set_not_after(&Asn1Time::days_from_now(1).unwrap())
        .unwrap();
    root.append_extension(BasicConstraints::new().critical().ca().build().unwrap())
        .unwrap();
    root.sign(&root_key, MessageDigest::sha256()).unwrap();
    let root = root.build();

    let leaf_key = PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap();
    let mut leaf_name = X509NameBuilder::new().unwrap();
    leaf_name.append_entry_by_text("CN", "localhost").unwrap();
    let leaf_name = leaf_name.build();
    let mut leaf = X509::builder().unwrap();
    leaf.set_version(2).unwrap();
    leaf.set_serial_number(&BigNum::from_u32(2).unwrap().to_asn1_integer().unwrap())
        .unwrap();
    leaf.set_subject_name(&leaf_name).unwrap();
    leaf.set_issuer_name(root.subject_name()).unwrap();
    leaf.set_pubkey(&leaf_key).unwrap();
    leaf.set_not_before(&Asn1Time::days_from_now(0).unwrap())
        .unwrap();
    leaf.set_not_after(&Asn1Time::days_from_now(1).unwrap())
        .unwrap();
    let san = SubjectAlternativeName::new()
        .dns("localhost")
        .build(&leaf.x509v3_context(Some(&root), None))
        .unwrap();
    leaf.append_extension(san).unwrap();
    leaf.sign(&root_key, MessageDigest::sha256()).unwrap();
    let mut chain = leaf.build().to_pem().unwrap();
    chain.extend(root.to_pem().unwrap());
    let identity =
        native_tls::Identity::from_pkcs8(&chain, &leaf_key.private_key_to_pem_pkcs8().unwrap())
            .unwrap();
    (String::from_utf8(root.to_pem().unwrap()).unwrap(), identity)
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn prepared_default_tls_preserves_private_ca_and_hostname_verification() {
    let (root, identity) = tls_fixture();
    let acceptor =
        tokio_native_tls::TlsAcceptor::from(native_tls::TlsAcceptor::new(identity).unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                if let Ok(mut socket) = acceptor.accept(socket).await {
                    let mut request = [0_u8; 4096];
                    let _ = socket.read(&mut request).await;
                    let _ = socket
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                        )
                        .await;
                }
            });
        }
    });

    let roots = BTreeMap::from([("ca_cert:fixture".to_owned(), root)]);
    let custom = HttpClients::build(&roots, Duration::from_secs(5));
    let trusted_url = format!("https://localhost:{port}/");
    // Both public and private pools must keep the real custom CA path.
    for client in [&custom.client, &custom.internal_client] {
        let response = client.get(&trusted_url).send().await.unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.text().await.unwrap(), "ok");
        assert!(
            client
                .get(format!("https://127.0.0.1:{port}/"))
                .send()
                .await
                .is_err(),
            "a trusted certificate must still match the hostname"
        );
    }
    let default = HttpClients::build(&BTreeMap::new(), Duration::from_secs(5));
    for client in [&default.client, &default.internal_client] {
        assert!(
            client.get(&trusted_url).send().await.is_err(),
            "prepared default roots must not trust another configuration's private CA"
        );
    }
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn prepared_default_tls_keeps_separate_engine_pools_and_timeouts() {
    use axum::{Router, extract::ConnectInfo, routing::get};
    use std::net::SocketAddr;

    let app = Router::new()
        .route(
            "/peer",
            get(|ConnectInfo(peer): ConnectInfo<SocketAddr>| async move { peer.to_string() }),
        )
        .route(
            "/slow",
            get(|| async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                "late"
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    let first = HttpClientCache::default().get(&BTreeMap::new(), Duration::from_secs(5));
    let second = HttpClientCache::default().get(&BTreeMap::new(), Duration::from_secs(5));
    let first_peer = first
        .client
        .get(format!("{base}/peer"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let first_again = first
        .client
        .get(format!("{base}/peer"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let second_peer = second
        .client
        .get(format!("{base}/peer"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(
        first_peer, first_again,
        "one pool should reuse its connection"
    );
    assert_ne!(first_peer, second_peer, "engines must not share HTTP pools");
    let short = HttpClients::build(&BTreeMap::new(), Duration::from_millis(10));
    for client in [&short.client, &short.internal_client] {
        let error = client.get(format!("{base}/slow")).send().await.unwrap_err();
        assert!(
            error.is_timeout(),
            "the configured request timeout must remain active"
        );
    }
    server.abort();
    let _ = server.await;
}
