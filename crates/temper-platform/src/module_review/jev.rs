//! Jev: one bounded call per state, a textual state and typed questions answered together.
//! Configured by [`JEV_URL_ENV`] and [`JEV_KEY_ENV`]; with either unset, no module is reviewed.
//!
//! Jev answers in well under three seconds nearly every time (2026-09: fit calls averaged 0.3 s, the slowest of 169
//! took 2.1 s), but now and then a request hangs until its timeout. So a second request starts when the first
//! fails in a way worth retrying or has not answered after [`HEDGE`], and the first answer wins: at most two requests,
//! never a retry loop. Out of credit (HTTP 402) is said, not retried.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::types::{Answer, Answers, Judge, JudgeUnavailable, Question};

/// The Jev release the item statements were calibrated on (26 Oct 2026).
pub const JEV_MODEL: &str = "jev-1.13.0";

/// How long one request may take: more than twice the slowest answer seen, so a request that runs out has hung.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// When a second request starts if the first has not answered: past the slowest answer seen.
const HEDGE: Duration = Duration::from_millis(2_500);

/// Where Jev is asked: the service's own endpoint, or a route that holds the key and its budgets, so a Temper that
/// should not hold the key asks with a pass of its own.
pub const JEV_URL_ENV: &str = "TEMPER_JEV_URL";

/// The bearer sent to [`JEV_URL_ENV`]: the service's key, or the route's pass. Never logged.
pub const JEV_KEY_ENV: &str = "TEMPER_JEV_KEY";

/// The module an ask is about (its kernel name), sent beside it so a route that holds the key can refuse an ask about
/// a module the app does not have. The service itself ignores it.
pub const MODULE_HEADER: &str = "temper-review-module";

/// Jev over HTTPS. The key is never logged or shown: `Debug` leaves it out.
pub struct JevJudge {
    client: reqwest::Client,
    url: String,
    model: String,
    key: Option<String>,
}

impl std::fmt::Debug for JevJudge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JevJudge")
            .field("url", &self.url)
            .field("model", &self.model)
            .field("key", &self.key.as_ref().map(|_| "set"))
            .finish()
    }
}

#[derive(Serialize)]
struct Request<'a> {
    state: &'a str,
    model: &'a str,
    questions: &'a BTreeMap<String, Question>,
}

/// The service's answer, or a route's in the same shape: the route says the tokens at the top (`input_tokens`).
#[derive(Deserialize)]
struct Response {
    answers: BTreeMap<String, Answer>,
    #[serde(default)]
    usage: Option<Usage>,
    #[serde(default)]
    input_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct Usage {
    #[serde(default)]
    input_tokens: u64,
}

/// One request's outcome. A failure carries its kind ("HTTP 503", "timeout"), never a response body.
enum Sent {
    Answer(Answers),
    Failed { kind: String, retry: bool },
}

impl JevJudge {
    /// Jev as the process environment names it ([`JEV_URL_ENV`], [`JEV_KEY_ENV`]), or `None` when either is unset:
    /// there is no default endpoint.
    pub fn from_env() -> Option<Self> {
        let set = |name: &str| {
            std::env::var(name)
                .ok()
                .filter(|value| !value.trim().is_empty())
        };
        let url = set(JEV_URL_ENV)?;
        let key = set(JEV_KEY_ENV)?;
        Some(Self::at(&url, Some(key)))
    }

    /// Jev at `url` with `key`; no key makes every ask [`JudgeUnavailable::NoKey`].
    pub fn at(url: &str, key: Option<String>) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .unwrap_or_default(),
            url: url.to_string(),
            model: JEV_MODEL.to_string(),
            key: key.filter(|key| !key.trim().is_empty()),
        }
    }

    async fn send(&self, key: &str, module: &str, body: &[u8]) -> Sent {
        let response = self
            .client
            .post(&self.url)
            .bearer_auth(key)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(MODULE_HEADER, module)
            .body(body.to_vec())
            .send()
            .await;
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                let kind = if error.is_timeout() {
                    "timeout"
                } else {
                    "network"
                };
                return Sent::Failed {
                    kind: kind.to_string(),
                    retry: true,
                };
            }
        };
        let status = response.status().as_u16();
        if !response.status().is_success() {
            let retry = matches!(status, 429 | 499 | 500 | 502 | 503 | 504);
            return Sent::Failed {
                kind: format!("HTTP {status}"),
                retry,
            };
        }
        match response.json::<Response>().await {
            Ok(parsed) => Sent::Answer(Answers {
                answers: parsed.answers,
                input_tokens: parsed
                    .usage
                    .map(|usage| usage.input_tokens)
                    .or(parsed.input_tokens)
                    .unwrap_or(0),
            }),
            Err(_) => Sent::Failed {
                kind: "no answers".to_string(),
                retry: false,
            },
        }
    }
}

/// The words for requests that all failed: out of credit when any said so.
fn unavailable(kinds: &[String]) -> JudgeUnavailable {
    if kinds.iter().any(|kind| kind == "HTTP 402") {
        return JudgeUnavailable::OutOfCredit;
    }
    JudgeUnavailable::Down(kinds.join(", "))
}

#[async_trait::async_trait]
impl Judge for JevJudge {
    async fn ask(
        &self,
        module: &str,
        state: &str,
        questions: &BTreeMap<String, Question>,
    ) -> Result<Answers, JudgeUnavailable> {
        let Some(key) = self.key.as_deref() else {
            return Err(JudgeUnavailable::NoKey);
        };
        let request = Request {
            state,
            model: &self.model,
            questions,
        };
        let body =
            serde_json::to_vec(&request).map_err(|e| JudgeUnavailable::Down(e.to_string()))?;

        let first = self.send(key, module, &body);
        tokio::pin!(first);
        let early = tokio::select! {
            sent = &mut first => Some(sent),
            () = tokio::time::sleep(HEDGE) => None,
        };
        let mut kinds = Vec::with_capacity(2);
        let first_out = match early {
            Some(Sent::Answer(answers)) => return Ok(answers),
            Some(Sent::Failed { kind, retry: false }) => return Err(unavailable(&[kind])),
            Some(Sent::Failed { kind, retry: true }) => {
                kinds.push(kind);
                true
            }
            None => false,
        };
        let second = self.send(key, module, &body);
        tokio::pin!(second);
        let (mut first_done, mut second_done) = (first_out, false);
        while !(first_done && second_done) {
            let sent = tokio::select! {
                sent = &mut first, if !first_done => { first_done = true; sent }
                sent = &mut second, if !second_done => { second_done = true; sent }
            };
            match sent {
                Sent::Answer(answers) => return Ok(answers),
                Sent::Failed { kind, .. } => kinds.push(kind),
            }
        }
        debug_assert!(!kinds.is_empty() && kinds.len() <= 2);
        Err(unavailable(&kinds))
    }
}
