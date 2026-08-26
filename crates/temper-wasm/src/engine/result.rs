//! Strict decoding for bounded terminal guest results.

use serde::Deserialize;
use temper_failure::GuestFailureDeclarationV1;

use crate::types::WasmInvocationResult;

use super::InvalidGuestResultKind;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SuccessResult {
    action: String,
    params: serde_json::Value,
    success: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyFailureResult {
    action: String,
    params: serde_json::Value,
    success: bool,
    error: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TypedFailureResult {
    success: bool,
    typed_failure: GuestFailureDeclarationV1,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum TerminalResult {
    TypedFailure(TypedFailureResult),
    LegacyFailure(LegacyFailureResult),
    Success(SuccessResult),
}

/// Decode one exact success, legacy-failure, or typed-failure wire shape.
pub(super) fn decode_terminal_result(
    result_json: &str,
    duration_ms: u64,
) -> Result<WasmInvocationResult, InvalidGuestResultKind> {
    serde_json::from_str::<serde_json::Value>(result_json)
        .map_err(|_| InvalidGuestResultKind::InvalidJson)?;
    let terminal = serde_json::from_str::<TerminalResult>(result_json)
        .map_err(|_| InvalidGuestResultKind::InvalidShape)?;

    match terminal {
        TerminalResult::Success(result) => {
            if !result.success {
                return Err(InvalidGuestResultKind::InvalidShape);
            }
            Ok(WasmInvocationResult {
                callback_action: result.action,
                callback_params: result.params,
                success: true,
                error: None,
                typed_failure: None,
                duration_ms,
            })
        }
        TerminalResult::LegacyFailure(result) => {
            if result.success
                || result.action != "callback"
                || result.params != serde_json::json!({"error": result.error})
            {
                return Err(InvalidGuestResultKind::InvalidShape);
            }
            Ok(WasmInvocationResult {
                callback_action: result.action,
                callback_params: result.params,
                success: false,
                error: Some(result.error),
                typed_failure: None,
                duration_ms,
            })
        }
        TerminalResult::TypedFailure(result) => {
            if result.success {
                return Err(InvalidGuestResultKind::InvalidShape);
            }
            Ok(WasmInvocationResult {
                callback_action: String::new(),
                callback_params: serde_json::Value::Null,
                success: false,
                error: None,
                typed_failure: Some(result.typed_failure),
                duration_ms,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use temper_failure::{FailureCategory, FailureOutcome, FailureRetryability};

    #[test]
    fn exact_terminal_shapes_decode() {
        let success = decode_terminal_result(
            r#"{"action":"Done","params":{"value":1},"success":true}"#,
            7,
        )
        .expect("valid success");
        assert!(success.success);
        assert_eq!(success.callback_action, "Done");

        let side_effect_only = decode_terminal_result(
            r#"{"action":"","params":{"stored":true},"success":true}"#,
            7,
        )
        .expect("valid side-effect-only success");
        assert!(side_effect_only.success);
        assert!(side_effect_only.callback_action.is_empty());
        assert_eq!(
            side_effect_only.callback_params,
            serde_json::json!({"stored": true})
        );

        let legacy = decode_terminal_result(
            r#"{"action":"callback","error":"failed","params":{"error":"failed"},"success":false}"#,
            8,
        )
        .expect("valid legacy failure");
        assert!(!legacy.success);
        assert_eq!(legacy.error.as_deref(), Some("failed"));

        let typed = decode_terminal_result(
            r#"{"success":false,"typed_failure":{"version":1,"category":"budget","code":"QuotaExhausted","retryability":"never","outcome":"not_applied","details":{}}}"#,
            9,
        )
        .expect("valid typed failure");
        let declaration = typed.typed_failure.expect("typed declaration");
        assert_eq!(declaration.category, FailureCategory::Budget);
        assert_eq!(declaration.retryability, FailureRetryability::Never);
        assert_eq!(declaration.outcome, FailureOutcome::NotApplied);
    }

    #[test]
    fn contradictory_unknown_and_injected_shapes_fail_closed() {
        let invalid = [
            r#"{"action":"Done","params":{},"success":false}"#,
            r#"{"action":"callback","error":"a","params":{"error":"b"},"success":false}"#,
            r#"{"success":false,"typed_failure":{"version":1,"category":"budget","code":"QuotaExhausted","retryability":"never","outcome":"not_applied","details":{}},"action":"Forged"}"#,
            r#"{"success":false,"typed_failure":{"version":1,"category":"budget","code":"QuotaExhausted","retryability":"never","outcome":"not_applied","details":{},"operation":{"id":"forged"}}}"#,
            r#"{"action":"Done","params":{},"success":true,"unknown":1}"#,
            r#"{"action":"Done","params":{},"success":true,"success":false}"#,
            r#"{"success":false,"typed_failure":{"version":2,"category":"budget","code":"QuotaExhausted","retryability":"never","outcome":"not_applied","details":{}}}"#,
            r#"{"success":false,"typed_failure":{"version":1,"category":"future","code":"QuotaExhausted","retryability":"never","outcome":"not_applied","details":{}}}"#,
        ];
        for encoded in invalid {
            assert!(
                matches!(
                    decode_terminal_result(encoded, 1),
                    Err(InvalidGuestResultKind::InvalidShape)
                ),
                "accepted {encoded}"
            );
        }
    }

    #[test]
    fn invalid_json_is_distinct_from_an_invalid_shape() {
        assert!(matches!(
            decode_terminal_result("not-json", 1),
            Err(InvalidGuestResultKind::InvalidJson)
        ));
    }
}
