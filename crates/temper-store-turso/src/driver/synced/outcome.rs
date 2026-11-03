//! What the remote said to each push, counted for monitoring.
//!
//! The local file takes every write first, so the store's own write metrics
//! keep counting successes while the remote refuses them all: on 3 Nov 2026
//! staging's Turso org answered every push `BLOCKED` for 38 minutes and
//! `temper_turso_query_duration{kind:execute,success:true}` never moved. The
//! positive signal is the remote's own answer to each push.

use std::sync::OnceLock;

use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;

use super::REMOTE_STATEMENT_FAILED;

/// The engine's text for an answer whose HTTP status was not 200.
const REMOTE_STATUS: &str = "unexpected http response: status=";

/// The longest code taken from an answer. Hrana's codes (`BLOCKED`,
/// `SQLITE_CONSTRAINT_PRIMARYKEY`, ...) fit well inside it.
const CODE_MAX_LEN: usize = 40;

/// The code of a refusal whose answer carried none that looks like one.
const CODE_UNRECOGNISED: &str = "UNRECOGNISED";

/// How one push to the remote ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum PushOutcome {
    /// The remote committed the changes the push sent.
    Confirmed,
    /// The remote answered and refused the push.
    Refused(RefusalCode),
    /// No answer: the connection failed or the attempt ran out of time.
    Failed,
}

/// The code a refusal carries, as a metric attribute.
///
/// Its values are bounded so the series stay few: the code from the
/// remote's answer when it is one (Hrana's codes are a fixed set of upper
/// case names, a few dozen), `HTTP_<status>` for an answer whose status was
/// not 200 and carried no code, and [`CODE_UNRECOGNISED`] for anything else.
/// Never the message: it carries row values and plan text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RefusalCode(String);

impl RefusalCode {
    /// The code as the metric's `code` attribute carries it.
    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

impl PushOutcome {
    /// What the engine's error for a push says the remote answered.
    ///
    /// The engine formats a statement the remote refused as
    /// `failed to execute sql: Error { message: "...", code: "BLOCKED" }`
    /// and an answer with another HTTP status as
    /// `unexpected http response: status=403, body=...`. Every other error
    /// (a connection refused or reset, a name not found) is no answer.
    pub(super) fn of_engine_error(message: &str) -> Self {
        if message.contains(REMOTE_STATEMENT_FAILED) {
            let code = quoted_code(message).unwrap_or(CODE_UNRECOGNISED);
            return Self::Refused(RefusalCode(code.to_string()));
        }
        if let Some(at) = message.find(REMOTE_STATUS) {
            let answer = &message[at + REMOTE_STATUS.len()..];
            let code = match quoted_code(answer) {
                Some(code) => code.to_string(),
                None => http_status_code(answer),
            };
            return Self::Refused(RefusalCode(code));
        }
        Self::Failed
    }

    /// Count this outcome in `temper_turso_sync_pushes`.
    ///
    /// The metric carries no Temper or environment of its own: the process's
    /// OpenTelemetry resource does (`deployment.environment.name` from
    /// `DD_ENV`, and whatever `OTEL_RESOURCE_ATTRIBUTES` names, such as the
    /// `app_id` and `temper_id` a Katagami install sets), as for every other
    /// metric this process sends.
    pub(super) fn record(&self) {
        let attributes = match self {
            Self::Confirmed => vec![KeyValue::new("outcome", "confirmed")],
            Self::Refused(code) => vec![
                KeyValue::new("outcome", "refused"),
                KeyValue::new("code", code.as_str().to_string()),
            ],
            Self::Failed => vec![KeyValue::new("outcome", "failed")],
        };
        pushes().add(1, &attributes);
    }
}

fn pushes() -> &'static Counter<u64> {
    static PUSHES: OnceLock<Counter<u64>> = OnceLock::new();
    PUSHES.get_or_init(|| {
        opentelemetry::global::meter("temper-store-turso")
            .u64_counter("temper_turso_sync_pushes")
            .with_description(
                "Pushes of a synced database's changes to its remote copy, by how the remote \
                 answered: outcome confirmed (it committed them), refused (it answered no; code \
                 is its answer's code, e.g. BLOCKED) or failed (no answer). A push with nothing \
                 to send is not counted.",
            )
            .build()
    })
}

/// The code in `code: "X"` (the engine's form) or `"code":"X"` (an answer's
/// JSON), the last one in `text`, when it looks like a code.
fn quoted_code(text: &str) -> Option<&str> {
    let start = ["code: \"", "\"code\":\""]
        .iter()
        .filter_map(|marker| text.rfind(marker).map(|at| at + marker.len()))
        .max()?;
    let rest = &text[start..];
    let code = &rest[..rest.find('"')?];
    let looks_like_a_code = !code.is_empty()
        && code.len() <= CODE_MAX_LEN
        && code
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_');
    looks_like_a_code.then_some(code)
}

/// `HTTP_<status>` for the three digits that start `answer`.
fn http_status_code(answer: &str) -> String {
    let status = answer.get(..3).unwrap_or_default();
    if status.len() == 3 && status.bytes().all(|byte| byte.is_ascii_digit()) {
        format!("HTTP_{status}")
    } else {
        CODE_UNRECOGNISED.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(code: &str) -> PushOutcome {
        PushOutcome::Refused(RefusalCode(code.to_string()))
    }

    #[test]
    fn a_refused_statement_carries_its_code() {
        let message = "Database sync engine error: failed to execute sql: Error { message: \
                       \"SQL write operations are forbidden (writes are blocked, do you need to \
                       upgrade your plan?)\", code: \"BLOCKED\" }";
        assert_eq!(PushOutcome::of_engine_error(message), refused("BLOCKED"));
    }

    #[test]
    fn a_code_inside_the_message_is_not_taken_for_the_answers() {
        let message = "failed to execute sql: Error { message: \"bad value code: \\\"X\\\"\", \
                       code: \"SQLITE_CONSTRAINT\" }";
        assert_eq!(
            PushOutcome::of_engine_error(message),
            refused("SQLITE_CONSTRAINT")
        );
    }

    #[test]
    fn a_status_answer_carries_its_code_or_its_status() {
        let coded = "sql_execute_http: unexpected http response: status=400, \
                     body={\"message\":\"blocked\",\"code\":\"BLOCKED\"}";
        assert_eq!(PushOutcome::of_engine_error(coded), refused("BLOCKED"));
        let bare = "sql_execute_http: unexpected http response: status=401, body=unauthorized";
        assert_eq!(PushOutcome::of_engine_error(bare), refused("HTTP_401"));
    }

    #[test]
    fn a_code_that_is_not_one_is_unrecognised() {
        let long = format!(
            "failed to execute sql: Error {{ message: \"x\", code: \"{}\" }}",
            "A".repeat(CODE_MAX_LEN + 1)
        );
        assert_eq!(
            PushOutcome::of_engine_error(&long),
            refused(CODE_UNRECOGNISED)
        );
        let lower = "failed to execute sql: Error { message: \"x\", code: \"blocked\" }";
        assert_eq!(
            PushOutcome::of_engine_error(lower),
            refused(CODE_UNRECOGNISED)
        );
    }

    #[test]
    fn an_error_with_no_answer_is_failed() {
        let message = "Database sync engine error: http error: connection refused";
        assert_eq!(PushOutcome::of_engine_error(message), PushOutcome::Failed);
    }
}
