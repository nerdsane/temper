//! Params an action does not declare (nerdsane/temper#351, ARN-247), reported
//! before they are refused.
//!
//! The runtime writes every key of an action's body into fields, so a caller
//! can change fields outside the action's verified contract. Specs with
//! `strict_action_params` already refuse such keys. For every other spec,
//! `TEMPER_UNDECLARED_PARAMS` chooses:
//!
//! - `report` (the default): nothing changes; each undeclared key is counted in
//!   `temper_undeclared_action_params_total` by type, action and kind, and the
//!   keys are named in a warning the first time a type and action get that kind,
//!   so the apps that still send them can be found;
//! - `enforce`: undeclared keys are dropped on every dispatch path, and an
//!   outside call that sends one is answered 400 after Cedar has decided it.

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::sync::{Mutex, OnceLock};

use temper_jit::TransitionTable;
use temper_jit::params::{ParamContractError, restrict_to_declared_params, undeclared_param_keys};

/// Keys the kernel adds to an integration's callback, which an app's spec may
/// not declare: counted as `kind="kernel"` so the report separates them from
/// keys an app's own code sends.
const KERNEL_CALLBACK_KEYS: &[&str] = &[
    "error",
    "error_message",
    "integration",
    "duration_ms",
    "trigger_action",
    "decision_id",
    "authz_denied",
];

/// What happens to a key an action does not declare.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Report,
    Enforce,
}

impl Mode {
    fn label(self) -> &'static str {
        match self {
            Mode::Report => "report",
            Mode::Enforce => "enforce",
        }
    }
}

/// `TEMPER_UNDECLARED_PARAMS`, read once: `enforce`, or else report.
pub fn mode() -> Mode {
    static MODE: OnceLock<Mode> = OnceLock::new();
    *MODE.get_or_init(|| {
        match std::env::var("TEMPER_UNDECLARED_PARAMS") // determinism-ok: read once at startup
            .map(|value| value.trim().to_ascii_lowercase())
            .as_deref()
        {
            Ok("enforce") => Mode::Enforce,
            _ => Mode::Report,
        }
    })
}

/// The params an action of a non-strict spec acts on: all of them when
/// reporting, the declared ones when enforcing. A body that is ambiguous (both
/// spellings of a parameter, different values) fails when enforcing.
pub(crate) fn screen<'a>(
    table: &TransitionTable,
    entity_type: &str,
    action: &str,
    params: &'a serde_json::Value,
) -> Result<Cow<'a, serde_json::Value>, String> {
    if table.strict_action_params {
        return Ok(Cow::Borrowed(params));
    }
    match undeclared_param_keys(table, action, params) {
        Ok(keys) => {
            let (kernel, sent): (Vec<String>, Vec<String>) = keys
                .into_iter()
                .partition(|key| KERNEL_CALLBACK_KEYS.contains(&key.as_str()));
            record(entity_type, action, "kernel", &kernel);
            record(entity_type, action, "undeclared", &sent);
        }
        Err(error) => record(entity_type, action, "ambiguous", &[error.to_string()]),
    }
    match mode() {
        Mode::Report => Ok(Cow::Borrowed(params)),
        Mode::Enforce => {
            restrict_to_declared_params(table, action, params).map_err(|error| error.to_string())
        }
    }
}

/// When enforcing, the 400 an outside call gets for a body its action does not
/// declare: its OData error code and message. Run after Cedar has decided the
/// call, so an unauthorized caller cannot read an action's contract from it.
pub(crate) fn external_refusal(
    table: &TransitionTable,
    action: &str,
    body: &serde_json::Value,
) -> Option<(&'static str, String)> {
    if mode() != Mode::Enforce || table.strict_action_params {
        return None;
    }
    match undeclared_param_keys(table, action, body) {
        Ok(keys) if keys.is_empty() => None,
        Ok(keys) => Some((
            "UndeclaredActionParams",
            format!(
                "Action '{action}' does not declare: {}. Send only the params its spec lists.",
                keys.join(", ")
            ),
        )),
        Err(error @ ParamContractError::AmbiguousAlias { .. }) => {
            Some(("AmbiguousActionParams", error.to_string()))
        }
        Err(error) => Some(("ActionParamContract", error.to_string())),
    }
}

/// Count the keys, and name them in a warning the first time this process sees
/// that kind on this type and action (so the warnings are bounded by the
/// specs, whatever callers send). Key names come from callers' code, never
/// values, which may be secrets.
fn record(entity_type: &str, action: &str, kind: &'static str, keys: &[String]) {
    if keys.is_empty() {
        return;
    }
    let mode = mode();
    crate::runtime_metrics::record_undeclared_action_params(
        entity_type,
        action,
        kind,
        mode.label(),
        keys.len() as u64,
    );
    static SEEN: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
    let seen = SEEN.get_or_init(Default::default);
    let first = format!("{entity_type}.{action}:{kind}");
    if seen.lock().is_ok_and(|mut seen| seen.insert(first)) {
        tracing::warn!(
            entity_type,
            action,
            kind,
            keys = %keys.join(","),
            mode = mode.label(),
            "action received params it does not declare"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn table(strict: bool) -> TransitionTable {
        TransitionTable::from_ioa_source(&format!(
            r#"
[automaton]
name = "Day"
states = ["Fetching", "Deciding"]
initial = "Fetching"
strict_action_params = {strict}

[[action]]
name = "Fetched"
kind = "input"
from = ["Fetching"]
to = "Deciding"
params = ["status_code"]
"#
        ))
    }

    #[test]
    fn reporting_leaves_the_params_as_sent() {
        assert_eq!(mode(), Mode::Report, "the default is report");
        let body = json!({ "status_code": 200, "response": { "daily": [] } });
        let screened = screen(&table(false), "Day", "Fetched", &body).unwrap();
        assert_eq!(screened.as_ref(), &body);
        assert!(external_refusal(&table(false), "Fetched", &body).is_none());
    }

    #[test]
    fn strict_specs_are_left_to_their_own_check() {
        let body = json!({ "status_code": 200, "response": 1 });
        assert!(matches!(
            screen(&table(true), "Day", "Fetched", &body),
            Ok(Cow::Borrowed(_))
        ));
    }
}
