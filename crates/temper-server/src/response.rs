//! OData response formatting (JSON, XML, and binary stream).

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

/// An OData stream response for `$value` endpoints.
///
/// Returns raw bytes with appropriate Content-Type, Content-Length, and optional ETag headers.
/// Used for binary content streams (file uploads/downloads via HasStream entities).
pub struct ODataStreamResponse {
    /// HTTP status code for the response.
    pub status: StatusCode,
    /// Raw binary body payload.
    pub body: Vec<u8>,
    /// MIME type for the Content-Type header.
    pub content_type: String,
    /// Optional ETag for cache validation (typically the content hash).
    pub etag: Option<String>,
}

impl IntoResponse for ODataStreamResponse {
    fn into_response(self) -> Response {
        let mut response = Response::builder()
            .status(self.status)
            .header("Content-Type", self.content_type)
            .header("Content-Length", self.body.len().to_string())
            .header("OData-Version", "4.0")
            .body(axum::body::Body::from(self.body))
            .expect("valid response"); // ci-ok: static headers always valid
        if let Some(etag) = self.etag {
            response.headers_mut().insert(
                "ETag",
                HeaderValue::from_str(&format!("\"{etag}\""))
                    .unwrap_or_else(|_| HeaderValue::from_static("\"unknown\"")),
            );
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;

    #[test]
    fn odata_error_format() {
        let resp = odata_error(
            StatusCode::NOT_FOUND,
            "EntityNotFound",
            "Order 123 not found",
        );
        assert_eq!(resp.status, StatusCode::NOT_FOUND);
        assert_eq!(resp.body["error"]["code"], "EntityNotFound");
        assert_eq!(resp.body["error"]["message"], "Order 123 not found");
    }

    #[test]
    fn odata_json_response_headers() {
        let resp = ODataResponse {
            status: StatusCode::OK,
            body: serde_json::json!({"value": []}),
        };
        let response = resp.into_response();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers().get("OData-Version").unwrap(), "4.0");
        assert!(
            response
                .headers()
                .get("Content-Type")
                .unwrap()
                .to_str()
                .unwrap()
                .contains("application/json")
        );
    }

    #[test]
    fn odata_xml_response_headers() {
        let resp = ODataXmlResponse {
            body: "<edmx:Edmx/>".to_string(),
        };
        let response = resp.into_response();
        assert_eq!(
            response.headers().get("Content-Type").unwrap(),
            "application/xml"
        );
        assert_eq!(response.headers().get("OData-Version").unwrap(), "4.0");
    }

    #[test]
    fn odata_error_custom_status() {
        let resp = odata_error(StatusCode::FORBIDDEN, "AuthzDenied", "not allowed");
        let response = resp.into_response();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn odata_stream_response_headers() {
        let resp = ODataStreamResponse {
            status: StatusCode::OK,
            body: b"hello world".to_vec(),
            content_type: "application/octet-stream".to_string(),
            etag: Some("sha256:abc123".to_string()),
        };
        let response = resp.into_response();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("Content-Type").unwrap(),
            "application/octet-stream"
        );
        assert_eq!(response.headers().get("Content-Length").unwrap(), "11");
        assert_eq!(response.headers().get("OData-Version").unwrap(), "4.0");
        assert!(response
            .headers()
            .get("ETag")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("sha256:abc123"));
    }

    #[test]
    fn odata_stream_response_no_etag() {
        let resp = ODataStreamResponse {
            status: StatusCode::NO_CONTENT,
            body: vec![],
            content_type: "application/toml".to_string(),
            etag: None,
        };
        let response = resp.into_response();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(response.headers().get("ETag").is_none());
    }
}
