use super::*;
use std::io::Write;
use std::sync::{Arc, Mutex};
use tracing::instrument::WithSubscriber;

#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl Write for LogBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn subscriber(logs: LogBuffer) -> impl tracing::Subscriber {
    tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(move || logs.clone())
        .finish()
}

#[tokio::test]
async fn large_refusals_keep_their_body_headers_and_logged_reason() {
    for (status, level, message) in [
        (
            StatusCode::CONFLICT,
            "INFO",
            "endpoint admission action refused",
        ),
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "WARN",
            "endpoint admission action failed",
        ),
    ] {
        // Typed guard diagnostics can include large stored field values.
        let reason = format!("stored value: {}: refusal detail", "x".repeat(1024 * 1024));
        let response = odata_error(status, "ActionFailed", &reason).into_response();
        let (parts, body) = response.into_parts();
        let expected = to_bytes(body, usize::MAX).await.unwrap();
        let headers = parts.headers.clone();
        let logs = LogBuffer::default();
        let response = log_refusal_response(
            "repo-route",
            "target",
            "Targets('one')/Example.Serve",
            Response::from_parts(parts, Body::from(expected.clone())),
        )
        .with_subscriber(subscriber(logs.clone()))
        .await;
        assert_eq!(response.status(), status);
        assert_eq!(response.headers(), &headers);
        assert!(
            to_bytes(response.into_body(), usize::MAX).await.unwrap() == expected,
            "refusal body changed"
        );
        let logs = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        for value in [
            level,
            message,
            "repo-route",
            "target",
            "Targets('one')/Example.Serve",
            &reason,
        ] {
            assert!(
                logs.contains(value),
                "missing log detail: {}",
                &value[..value.len().min(100)]
            );
        }
    }
}

#[tokio::test]
async fn unreadable_refusal_logs_the_extraction_error_and_preserves_failure() {
    let body = Body::from_stream(futures_util::stream::once(async {
        Err::<Bytes, _>(std::io::Error::other("refusal stream broke"))
    }));
    let response = Response::builder()
        .status(StatusCode::CONFLICT)
        .body(body)
        .unwrap();
    let logs = LogBuffer::default();
    let response = log_refusal_response(
        "repo-route",
        "target",
        "Targets('one')/Example.Serve",
        response,
    )
    .with_subscriber(subscriber(logs.clone()))
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(to_bytes(response.into_body(), usize::MAX).await.is_err());
    let logs = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    for value in [
        "WARN",
        "repo-route",
        "target",
        "Targets('one')/Example.Serve",
        "refusal stream broke",
    ] {
        assert!(logs.contains(value), "missing log detail: {value}");
    }
}
