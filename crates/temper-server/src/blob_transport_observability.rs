use std::time::Instant;

use reqwest::StatusCode;

#[derive(Debug)]
pub(crate) struct BlobTransportError {
    pub(crate) message: String,
    pub(crate) status: Option<StatusCode>,
}

pub(crate) struct BlobTransportFinish<'a> {
    pub(crate) started_at: Instant,
    pub(crate) span: &'a tracing::Span,
    pub(crate) operation: &'a str,
    pub(crate) backend: &'a str,
    pub(crate) outcome: &'a str,
    pub(crate) status: Option<StatusCode>,
    pub(crate) request_bytes: u64,
    pub(crate) response_bytes: u64,
}

impl BlobTransportError {
    pub(crate) fn message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            status: None,
        }
    }

    pub(crate) fn status(message: impl Into<String>, status: StatusCode) -> Self {
        Self {
            message: message.into(),
            status: Some(status),
        }
    }
}

pub(crate) fn blob_transport_span(
    operation: &'static str,
    backend: &'static str,
    request_bytes: u64,
) -> tracing::Span {
    match operation {
        "put" => tracing::info_span!(
            "blob.transport.put",
            otel.name = "blob.transport.put",
            blob.backend = backend,
            blob.operation = operation,
            request_bytes,
            response_bytes = tracing::field::Empty,
            outcome = tracing::field::Empty,
            status_code_class = tracing::field::Empty,
            http.status_code = tracing::field::Empty,
            duration_ms = tracing::field::Empty,
        ),
        "put_content" => tracing::info_span!(
            "blob.transport.put_content",
            otel.name = "blob.transport.put_content",
            blob.backend = backend,
            blob.operation = operation,
            request_bytes,
            response_bytes = tracing::field::Empty,
            outcome = tracing::field::Empty,
            status_code_class = tracing::field::Empty,
            http.status_code = tracing::field::Empty,
            duration_ms = tracing::field::Empty,
        ),
        "get" => tracing::info_span!(
            "blob.transport.get",
            otel.name = "blob.transport.get",
            blob.backend = backend,
            blob.operation = operation,
            request_bytes,
            response_bytes = tracing::field::Empty,
            outcome = tracing::field::Empty,
            status_code_class = tracing::field::Empty,
            http.status_code = tracing::field::Empty,
            duration_ms = tracing::field::Empty,
        ),
        "head" => tracing::info_span!(
            "blob.transport.head",
            otel.name = "blob.transport.head",
            blob.backend = backend,
            blob.operation = operation,
            request_bytes,
            response_bytes = tracing::field::Empty,
            outcome = tracing::field::Empty,
            status_code_class = tracing::field::Empty,
            http.status_code = tracing::field::Empty,
            duration_ms = tracing::field::Empty,
        ),
        _ => tracing::info_span!(
            "blob.transport",
            otel.name = "blob.transport",
            blob.backend = backend,
            blob.operation = operation,
            request_bytes,
            response_bytes = tracing::field::Empty,
            outcome = tracing::field::Empty,
            status_code_class = tracing::field::Empty,
            http.status_code = tracing::field::Empty,
            duration_ms = tracing::field::Empty,
        ),
    }
}

pub(crate) fn finish_blob_transport(record: BlobTransportFinish<'_>) {
    let duration = record.started_at.elapsed();
    let status_code_class = blob_status_code_class(record.status, record.outcome);
    record.span.record("response_bytes", record.response_bytes);
    record.span.record("outcome", record.outcome);
    record.span.record("status_code_class", status_code_class);
    if let Some(status) = record.status {
        record.span.record("http.status_code", status.as_u16());
    }
    record
        .span
        .record("duration_ms", duration.as_secs_f64() * 1000.0);
    crate::runtime_metrics::blob_transport::record(
        duration,
        record.operation,
        record.backend,
        record.outcome,
        status_code_class,
        record.request_bytes,
        record.response_bytes,
    );
}

fn blob_status_code_class(status: Option<StatusCode>, outcome: &str) -> &'static str {
    match status {
        Some(status) if status.is_success() => "2xx",
        Some(status) if status.is_client_error() => "4xx",
        Some(status) if status.is_server_error() => "5xx",
        Some(_) => "error",
        None if outcome == "error" => "error",
        None => "none",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_status_code_class_has_bounded_tags() {
        assert_eq!(blob_status_code_class(Some(StatusCode::OK), "ok"), "2xx");
        assert_eq!(
            blob_status_code_class(Some(StatusCode::NOT_FOUND), "not_found"),
            "4xx"
        );
        assert_eq!(
            blob_status_code_class(Some(StatusCode::INTERNAL_SERVER_ERROR), "error"),
            "5xx"
        );
        assert_eq!(blob_status_code_class(None, "ok"), "none");
        assert_eq!(blob_status_code_class(None, "not_found"), "none");
        assert_eq!(blob_status_code_class(None, "error"), "error");
        assert_eq!(
            blob_status_code_class(Some(StatusCode::TEMPORARY_REDIRECT), "error"),
            "error"
        );
    }
}
