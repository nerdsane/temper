use axum::http::StatusCode;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, HeaderName};

const TEMPER_CLIENT_JS: &str = include_str!("../../static/temper-client.js");

pub(super) async fn serve_temper_client()
-> (StatusCode, [(HeaderName, &'static str); 2], &'static str) {
    (
        StatusCode::OK,
        [
            (CONTENT_TYPE, "application/javascript"),
            (CACHE_CONTROL, "public, max-age=3600"),
        ],
        TEMPER_CLIENT_JS,
    )
}
