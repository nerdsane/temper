//! OData JSON response formatting.

use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};

/// An OData-formatted JSON response.
pub struct ODataResponse {
    /// HTTP status code for the response.
    pub status: StatusCode,
    /// JSON body payload.
    pub body: serde_json::Value,
}

impl IntoResponse for ODataResponse {
    fn into_response(self) -> Response {
        let mut response = axum::Json(self.body).into_response();
        *response.status_mut() = self.status;
        response
            .headers_mut()
            .insert("OData-Version", HeaderValue::from_static("4.0"));
        response.headers_mut().insert(
            "Content-Type",
            HeaderValue::from_static("application/json;odata.metadata=minimal"),
        );
        response
    }
}

/// An OData error response following the OData JSON error format.
pub fn odata_error(status: StatusCode, code: &str, message: &str) -> ODataResponse {
    ODataResponse {
        status,
        body: serde_json::json!({
            "error": {
                "code": code,
                "message": message
            }
        }),
    }
}

/// An OData XML response (for $metadata).
pub struct ODataXmlResponse {
    /// The XML body content.
    pub body: String,
}

impl IntoResponse for ODataXmlResponse {
    fn into_response(self) -> Response {
        let mut response = self.body.into_response();
        response
            .headers_mut()
            .insert("Content-Type", HeaderValue::from_static("application/xml"));
        response
            .headers_mut()
            .insert("OData-Version", HeaderValue::from_static("4.0"));
        response
    }
}
