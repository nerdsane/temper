//! A guest's `Host` header never reaches an external server: the request
//! names the host the `http_call` gate checked, so a shared front end cannot
//! route it to another site. Drives the real `ProductionWasmHost` (reqwest)
//! against a loopback server that answers with the `Host` it received.

use std::collections::BTreeMap;

use axum::Router;
use axum::extract::Request;
use axum::http::header::HOST;
use axum::response::IntoResponse;
use temper_wasm::http_stream::HttpRequestHead;
use temper_wasm::{ProductionWasmHost, WasmHost};

#[tokio::test]
async fn a_guest_host_header_never_replaces_the_url_host() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new().fallback(|request: Request| async move {
        let host = request.headers()[HOST].to_str().unwrap().to_owned();
        if request.headers().contains_key("connect-protocol-version") {
            // One Connect data frame carrying the host as a JSON string.
            let payload = format!("\"{host}\"");
            let mut frame = vec![0];
            frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            frame.extend_from_slice(payload.as_bytes());
            frame.into_response()
        } else {
            host.into_response()
        }
    });
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let host = ProductionWasmHost::new(BTreeMap::new());
    let url = format!("http://{address}/");
    let expected = address.to_string();
    for name in ["Host", "HOST"] {
        let headers = [(name.to_owned(), "elsewhere.example".to_owned())];

        assert_eq!(
            host.http_call("GET", &url, &headers, "").await,
            Ok((200, expected.clone()))
        );
        assert_eq!(
            host.http_call_binary("GET", &url, &headers, &[]).await,
            Ok((200, expected.clone().into_bytes()))
        );
        assert_eq!(
            host.connect_call(&url, &headers, "{}").await,
            Ok(vec![format!("\"{expected}\"")])
        );

        let handles = host
            .http_stream_begin_outbound(HttpRequestHead {
                method: "GET".into(),
                url: url.clone(),
                headers: headers.to_vec(),
            })
            .await
            .unwrap();
        host.http_stream_close(handles.request_body).await.unwrap();
        let mut body = Vec::new();
        loop {
            let chunk = host.http_stream_read(handles.response_body).await.unwrap();
            if chunk.is_empty() {
                break;
            }
            body.extend(chunk);
        }
        assert_eq!(String::from_utf8(body).unwrap(), expected);
    }
}
