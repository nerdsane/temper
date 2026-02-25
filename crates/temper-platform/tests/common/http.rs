use axum::body::Body;

/// Read a response body as JSON.
pub async fn body_json(response: axum::http::Response<Body>) -> serde_json::Value {
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&body).unwrap()
}

/// Read a response body as UTF-8 string.
pub async fn body_string(response: axum::http::Response<Body>) -> String {
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    String::from_utf8(body.to_vec()).unwrap()
}
