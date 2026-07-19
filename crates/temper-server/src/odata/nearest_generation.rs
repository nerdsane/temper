use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::response::odata_error;

pub(super) fn generation_changed() -> Response {
    odata_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "VectorIndexRebuilding",
        "vector search crossed a specification generation change; retry against the current generation",
    )
    .into_response()
}
