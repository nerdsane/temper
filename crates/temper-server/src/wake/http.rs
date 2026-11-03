//! The control plane's end of the reports, over HTTP.

use std::time::Duration;

use reqwest::StatusCode;

use super::{SendError, WakeReport, WakeSink};

/// How long one report may take. A request the control plane never answered
/// would stay in flight for good, and the due time it carries would never be
/// sent again; one that times out is sent again, as any undelivered report is.
const SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// POSTs each report as JSON to the control plane's address with the
/// process's pass as a bearer token.
pub struct HttpWakeSink {
    client: reqwest::Client,
    url: String,
    token: String,
}

impl HttpWakeSink {
    /// Reports to `url`, authorized by `token`.
    pub fn new(url: String, token: String) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .timeout(SEND_TIMEOUT)
            .build()
            .map_err(|error| format!("wake report client: {error}"))?;
        Ok(Self { client, url, token })
    }
}

impl std::fmt::Debug for HttpWakeSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpWakeSink")
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl WakeSink for HttpWakeSink {
    async fn send(&self, report: &WakeReport, traceparent: Option<&str>) -> Result<(), SendError> {
        let mut request = self
            .client
            .post(&self.url)
            .bearer_auth(&self.token)
            .json(report);
        if let Some(traceparent) = traceparent {
            request = request.header("traceparent", traceparent);
        }
        let response = request
            .send()
            .await
            .map_err(|error| SendError::Undelivered(error.to_string()))?;
        answer(response.status())
    }
}

/// What a status means for the report. A 4xx other than 408 and 429 is the
/// control plane refusing this pass on purpose, and a refused pass is refused
/// again on every retry, so it is not retried. 408, 429 and 5xx are a control
/// plane that could not take the report now, and it is sent again.
fn answer(status: StatusCode) -> Result<(), SendError> {
    if status.is_success() {
        return Ok(());
    }
    let later = status == StatusCode::REQUEST_TIMEOUT || status == StatusCode::TOO_MANY_REQUESTS;
    if status.is_client_error() && !later {
        return Err(SendError::Refused(status.as_u16()));
    }
    Err(SendError::Undelivered(format!(
        "control plane answered {status}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refused_pass_is_not_retried_and_a_busy_control_plane_is() {
        assert_eq!(answer(StatusCode::OK), Ok(()));
        for refused in [400, 401, 403, 404, 410] {
            let status = StatusCode::from_u16(refused).unwrap();
            assert_eq!(answer(status), Err(SendError::Refused(refused)), "{status}");
        }
        for later in [408, 429, 500, 502, 503] {
            let status = StatusCode::from_u16(later).unwrap();
            assert!(
                matches!(answer(status), Err(SendError::Undelivered(_))),
                "{status}"
            );
        }
    }
}
