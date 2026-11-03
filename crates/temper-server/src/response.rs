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

/// An action refusal: `message` as callers read it, and the typed reason
/// beside it in the error object. A typed refusal names its own code (such as
/// `GuardFailed`); without one the refusal answers with `code`.
pub fn odata_refusal(
    status: StatusCode,
    code: &str,
    message: &str,
    refusal: Option<&temper_jit::table::Refusal>,
) -> ODataResponse {
    let mut response = odata_error(status, code, message);
    if let Some(refusal) = refusal {
        merge_refusal(&mut response.body["error"], refusal);
    }
    response
}

/// Write `refusal`'s code and fields into a JSON object, beside its message.
pub fn merge_refusal(target: &mut serde_json::Value, refusal: &temper_jit::table::Refusal) {
    let (Some(target), Ok(serde_json::Value::Object(fields))) =
        (target.as_object_mut(), serde_json::to_value(refusal))
    else {
        return;
    };
    target.extend(fields);
}

/// A resumable authorization denial with an authoritative decision identifier.
pub fn odata_denial(message: &str, decision_id: &str) -> ODataResponse {
    let mut response = odata_error(StatusCode::FORBIDDEN, "AuthorizationDenied", message);
    response.body["decision_id"] = serde_json::Value::String(decision_id.to_string());
    response
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
    fn denial_identity_survives_server_to_sandbox_round_trip() {
        let response = odata_denial("Order('PD-victim'). Decision PD-real created.", "PD-real");
        let encoded = serde_json::to_string(&response.body).unwrap();
        let parsed = temper_sandbox::helpers::format_authz_denied(&encoded).unwrap();
        assert_eq!(parsed["decision_id"], "PD-real");
    }

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
        assert!(
            response
                .headers()
                .get("ETag")
                .unwrap()
                .to_str()
                .unwrap()
                .contains("sha256:abc123")
        );
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
