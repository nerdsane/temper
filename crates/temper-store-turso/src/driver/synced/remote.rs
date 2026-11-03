//! Reading the remote's record of this file's pushes, outside the engine.
//!
//! The engine pushes only changes numbered above the remote's record for this
//! file (`turso_sync_last_change_id`), and never says what that record was.
//! Temper reads it itself to tell whether the file is behind the remote, and
//! whether a push the remote refused still moved it. The same request, made
//! on a new connection and timed step by step, shows where a hung push waits.

use std::fmt::Display;
use std::time::{Duration, Instant};

use super::DriverError;

/// One request for the record.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// How long each step of a timed fresh request may take. Past the 1 s
/// temper-cli lets a lost DNS answer cost (`RES_OPTIONS`), so one lost answer
/// shows as a slow lookup rather than a failed one; far past a fresh
/// request's normal 70 ms (lookup 9, connect 15, TLS 30, first byte 15 ms at
/// the median on 28 Oct 2026).
const STEP_TIMEOUT: Duration = Duration::from_secs(5);

/// A fresh request to the remote, timed step by step. A step that failed
/// has no time, and neither does any step after it.
#[derive(Debug, Default)]
pub(super) struct FreshRequest {
    /// The remote's host name.
    pub host: String,
    /// Looking the host name up, through the system resolver the engine's
    /// client uses too.
    pub lookup_ms: Option<u64>,
    /// A TCP connection to the first address found.
    pub connect_ms: Option<u64>,
    /// A new HTTP connection to that address and a request for the remote's
    /// record, until its answer's status line.
    pub answer_ms: Option<u64>,
    /// The answer's HTTP status.
    pub status: Option<u16>,
    /// The step that failed, and how.
    pub failed: Option<String>,
}

/// Where the remote is and who this file is to it.
pub(super) struct Remote {
    pipeline_url: String,
    auth_token: String,
    client_id: String,
    http: reqwest::Client,
}

impl Remote {
    pub(super) fn new(remote_url: &str, auth_token: &str, client_id: String) -> Self {
        let base = remote_url
            .trim()
            .trim_end_matches('/')
            .replacen("libsql://", "https://", 1)
            .replacen("turso://", "https://", 1);
        Self {
            pipeline_url: format!("{base}/v2/pipeline"),
            auth_token: auth_token.to_string(),
            client_id,
            http: reqwest::Client::new(),
        }
    }

    /// The request for the remote's record of this file's pushes.
    fn record_request(&self) -> serde_json::Value {
        serde_json::json!({
            "requests": [{
                "type": "execute",
                "stmt": {
                    "sql": "SELECT change_id FROM turso_sync_last_change_id WHERE client_id = ?",
                    "args": [{ "type": "text", "value": self.client_id }],
                },
            }],
        })
    }

    /// Ask the remote for its record on a new connection, timing the name
    /// lookup, the connection and the answer apart. It shares nothing with
    /// the engine's client, so it shows whether the remote can be found,
    /// reached and answers now, not what the engine's own request is
    /// waiting on.
    pub(super) async fn time_fresh_request(&self) -> FreshRequest {
        let mut fresh = FreshRequest::default();
        if let Err(failed) = self.time_steps(&mut fresh).await {
            fresh.failed = Some(failed);
        }
        fresh
    }

    async fn time_steps(&self, fresh: &mut FreshRequest) -> Result<(), String> {
        let url =
            reqwest::Url::parse(&self.pipeline_url).map_err(|error| format!("url: {error}"))?;
        let host = url.host_str().unwrap_or_default().to_string();
        fresh.host.clone_from(&host);
        let port = url.port_or_known_default().unwrap_or(443);
        let addr = step("lookup", &mut fresh.lookup_ms, async {
            tokio::net::lookup_host((host.as_str(), port))
                .await?
                .next()
                .ok_or_else(|| std::io::Error::other("no address"))
        })
        .await?;
        step(
            "connect",
            &mut fresh.connect_ms,
            tokio::net::TcpStream::connect(addr),
        )
        .await?;
        let client = reqwest::Client::builder()
            .resolve(&host, addr)
            .build()
            .map_err(|error| format!("client: {error}"))?;
        let request = client
            .post(url)
            .bearer_auth(&self.auth_token)
            .json(&self.record_request())
            .send();
        let answer = step("answer", &mut fresh.answer_ms, request).await?;
        fresh.status = Some(answer.status().as_u16());
        Ok(())
    }

    /// The number of the last change of this file the remote has applied; 0
    /// when it has none.
    pub(super) async fn last_change(&self) -> Result<i64, DriverError> {
        let body = self.record_request();
        let failed = |what: String| {
            DriverError::SyncSetup(format!("read the remote copy's progress: {what}"))
        };
        let answer: serde_json::Value = self
            .http
            .post(&self.pipeline_url)
            .bearer_auth(&self.auth_token)
            .json(&body)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|error| failed(error.to_string()))?
            .json()
            .await
            .map_err(|error| failed(error.to_string()))?;
        let result = &answer["results"][0];
        if result["type"] == "error" {
            let message = result["error"]["message"].as_str().unwrap_or_default();
            // Nothing has been pushed to this remote yet.
            if message.contains("no such table") {
                return Ok(0);
            }
            return Err(failed(message.to_string()));
        }
        // Any other answer is an error, never a 0: a 0 would turn off the
        // checks that read this.
        let unexpected = || failed(format!("unexpected answer: {result}"));
        if result["type"] != "ok" {
            return Err(unexpected());
        }
        let rows = result["response"]["result"]["rows"]
            .as_array()
            .ok_or_else(unexpected)?;
        // No row: nothing has been pushed from this file yet.
        let Some(value) = rows.first().map(|row| &row[0]) else {
            return Ok(0);
        };
        // Integers arrive as strings in the Hrana protocol.
        match &value["value"] {
            serde_json::Value::String(text) => text.parse().map_err(|_| unexpected()),
            serde_json::Value::Number(number) => number.as_i64().ok_or_else(unexpected),
            _ if value["type"] == "null" => Ok(0),
            _ => Err(unexpected()),
        }
    }
}

/// Run one step of a timed request within [`STEP_TIMEOUT`], keeping its time
/// when it succeeds.
async fn step<T, E: Display>(
    name: &str,
    took: &mut Option<u64>,
    work: impl Future<Output = Result<T, E>>,
) -> Result<T, String> {
    let started = Instant::now();
    match tokio::time::timeout(STEP_TIMEOUT, work).await {
        Ok(Ok(value)) => {
            *took = Some(started.elapsed().as_millis() as u64);
            Ok(value)
        }
        Ok(Err(error)) => Err(format!(
            "{name}: {error} after {} ms",
            started.elapsed().as_millis()
        )),
        Err(_) => Err(format!(
            "{name}: nothing within {} s",
            STEP_TIMEOUT.as_secs()
        )),
    }
}

/// The client id the engine gave this file, from its sync metadata.
pub(super) fn client_id(info: &serde_json::Value) -> String {
    info["client_unique_id"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}
