//! Reuse immutable TLS preparation in tests, never HTTP clients or request state.
//!
//! Even loopback HTTP fixtures construct a TLS connector. With pinned reqwest
//! 0.12, native TLS reparses the system CA bundle for each such construction.
//! Production constructors deliberately do not use this test-only module.

use std::sync::OnceLock;

fn connector() -> &'static native_tls::TlsConnector {
    static CONNECTOR: OnceLock<native_tls::TlsConnector> = OnceLock::new();
    CONNECTOR.get_or_init(|| {
        // Match pinned reqwest's default native TLS configuration: TLS 1.2
        // minimum, no maximum or client identity, SNI and certificate/hostname
        // verification enabled, and system roots. native-tls-alpn is not enabled
        // in this workspace, so neither original nor prepared connector sets ALPN.
        native_tls::TlsConnector::new().expect("test native TLS initialization")
    })
}

/// Prepare a fresh builder with the default TLS connector and a new HTTP pool.
pub(crate) fn builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder().use_preconfigured_tls(connector().clone())
}

/// Construct a fresh default client, preserving the `Client::new()` error policy.
pub(crate) fn client() -> reqwest::Client {
    builder().build().expect("Client::new()")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        extract::ConnectInfo,
        http::{HeaderMap, StatusCode, header},
        routing::get,
    };
    use serde_json::{Value, json};
    use std::{net::SocketAddr, time::Duration};

    async fn echo(
        ConnectInfo(peer): ConnectInfo<SocketAddr>,
        headers: HeaderMap,
    ) -> ([(header::HeaderName, &'static str); 1], Json<Value>) {
        let value = |name: &str| {
            headers
                .get(name)
                .map(|value| value.to_str().unwrap().to_owned())
        };
        (
            [(header::SET_COOKIE, "fixture=value")],
            Json(json!({
                "peer": peer.to_string(),
                "authorization": value("authorization"),
                "session": value("x-session-id"),
                "cookie": value("cookie"),
            })),
        )
    }

    #[tokio::test]
    async fn prepared_tls_keeps_fresh_client_pools_and_request_state() {
        assert!(std::ptr::eq(connector(), connector()));
        let app = Router::new().route("/echo", get(echo)).route(
            "/redirect",
            get(|| async { (StatusCode::FOUND, [(header::LOCATION, "/echo")], "") }),
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

        tokio::time::timeout(Duration::from_secs(5), async {
            let first = client();
            let second = client();
            let first_response: Value = first
                .get(format!("{base}/echo"))
                .bearer_auth("fixture-first")
                .header("X-Session-Id", "fixture-session")
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(first_response["authorization"], "Bearer fixture-first");
            assert_eq!(first_response["session"], "fixture-session");

            // Reading the entire response returns its connection to this pool.
            let first_again: Value = first
                .get(format!("{base}/echo"))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(first_again["peer"], first_response["peer"]);
            for field in ["authorization", "session", "cookie"] {
                assert!(first_again[field].is_null(), "retained {field}");
            }

            let second_response: Value = second
                .get(format!("{base}/echo"))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_ne!(second_response["peer"], first_response["peer"]);
            for field in ["authorization", "session", "cookie"] {
                assert!(second_response[field].is_null(), "shared {field}");
            }

            // The default redirect policy remains unchanged; callers can still
            // choose the private administration/upload policy on a fresh builder.
            let followed = second.get(format!("{base}/redirect")).send().await.unwrap();
            assert_eq!(followed.status(), StatusCode::OK);
            let private = builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap();
            let not_followed = private
                .get(format!("{base}/redirect"))
                .send()
                .await
                .unwrap();
            assert_eq!(not_followed.status(), StatusCode::FOUND);
        })
        .await
        .expect("independent client requests exceeded fixture deadline");
        server.abort();
    }
}
