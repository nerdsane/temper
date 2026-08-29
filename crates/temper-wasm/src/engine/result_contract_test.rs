//! Live Wasmtime coverage for terminal-result transport bounds and cardinality.

use super::tests::{make_context, make_host, make_streams};
use super::{InvalidGuestResultKind, WasmEngine, WasmError};
use crate::{MAX_WASM_RESULT_BYTES_V1, WasmInvocationResult, WasmResourceLimits};

const RESULT_OFFSET: usize = 8192;

fn wat_string(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len());
    for byte in bytes {
        match byte {
            b'"' => encoded.push_str("\\\""),
            b'\\' => encoded.push_str("\\\\"),
            0x20..=0x7e => encoded.push(char::from(*byte)),
            _ => encoded.push_str(&format!("\\{byte:02x}")),
        }
    }
    encoded
}

fn host_result_wat(payload: &[u8], calls: usize, return_value: i32) -> String {
    let calls = (0..calls)
        .map(|_| {
            format!(
                "i32.const {RESULT_OFFSET}\ni32.const {}\ncall $host_set_result",
                payload.len()
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        r#"(module
          (import "env" "host_set_result" (func $host_set_result (param i32 i32)))
          (memory (export "memory") 18)
          (data (i32.const {RESULT_OFFSET}) "{}")
          (func (export "run") (param i32 i32) (result i32)
            {calls}
            i32.const {return_value}))"#,
        wat_string(payload)
    )
}

fn pointer_result_wat(payload: &[u8]) -> String {
    let length = u32::try_from(payload.len())
        .expect("test payload length must fit the legacy u32 prefix")
        .to_le_bytes();
    let prefix_offset = RESULT_OFFSET - length.len();
    format!(
        r#"(module
          (memory (export "memory") 18)
          (data (i32.const {prefix_offset}) "{}")
          (data (i32.const {RESULT_OFFSET}) "{}")
          (func (export "run") (param i32 i32) (result i32)
            i32.const {RESULT_OFFSET}))"#,
        wat_string(&length),
        wat_string(payload)
    )
}

async fn invoke_wat(wat: &str) -> Result<WasmInvocationResult, WasmError> {
    let engine = WasmEngine::new().expect("create engine");
    let hash = engine
        .compile_and_cache(wat.as_bytes())
        .expect("compile test guest");
    engine
        .invoke(
            &hash,
            &make_context(),
            make_host(),
            &WasmResourceLimits::default(),
            make_streams(),
        )
        .await
}

fn success_payload_with_len(length: usize) -> Vec<u8> {
    let prefix = br#"{"action":"Done","params":""#;
    let suffix = br#"","success":true}"#;
    assert!(length >= prefix.len() + suffix.len());
    let mut payload = Vec::with_capacity(length);
    payload.extend_from_slice(prefix);
    payload.resize(length - suffix.len(), b'x');
    payload.extend_from_slice(suffix);
    assert_eq!(payload.len(), length);
    payload
}

#[tokio::test]
async fn typed_failure_decodes_identically_from_both_result_transports() {
    let payload = br#"{"success":false,"typed_failure":{"version":1,"category":"authorization","code":"ApprovalRequired","retryability":"after_authorization","outcome":"not_applied","diagnostic":"private","details":{"decision":{"kind":"string","value":"private"}}}}"#;

    let host_result = invoke_wat(&host_result_wat(payload, 1, 0))
        .await
        .expect("host result should decode");
    let pointer_result = invoke_wat(&pointer_result_wat(payload))
        .await
        .expect("pointer result should decode");

    assert_eq!(host_result.typed_failure, pointer_result.typed_failure);
    assert!(host_result.error.is_none());
    assert!(!host_result.success);
}

#[tokio::test]
async fn exact_result_budget_is_accepted_on_both_transports() {
    let payload = success_payload_with_len(MAX_WASM_RESULT_BYTES_V1);
    assert!(
        invoke_wat(&host_result_wat(&payload, 1, 0))
            .await
            .expect("exact host-write budget should be valid")
            .success
    );
    assert!(
        invoke_wat(&pointer_result_wat(&payload))
            .await
            .expect("exact pointer budget should be valid")
            .success
    );
}

#[tokio::test]
async fn both_transports_reject_budget_plus_one_before_payload_allocation() {
    let oversized_len = MAX_WASM_RESULT_BYTES_V1 + 1;
    let host_wat = format!(
        r#"(module
          (import "env" "host_set_result" (func $host_set_result (param i32 i32)))
          (memory (export "memory") 1)
          (func (export "run") (param i32 i32) (result i32)
            i32.const 0
            i32.const {oversized_len}
            call $host_set_result
            i32.const 0))"#
    );
    assert!(matches!(
        invoke_wat(&host_wat).await,
        Err(WasmError::InvalidGuestResult(
            InvalidGuestResultKind::ResultTooLarge
        ))
    ));

    let oversized_prefix = u32::try_from(oversized_len)
        .expect("budget plus one fits u32")
        .to_le_bytes();
    let pointer_wat = format!(
        r#"(module
          (memory (export "memory") 1)
          (data (i32.const 1020) "{}")
          (func (export "run") (param i32 i32) (result i32)
            i32.const 1024))"#,
        wat_string(&oversized_prefix)
    );
    assert!(matches!(
        invoke_wat(&pointer_wat).await,
        Err(WasmError::InvalidGuestResult(
            InvalidGuestResultKind::ResultTooLarge
        ))
    ));
}

#[tokio::test]
async fn zero_multiple_and_dual_result_sources_fail_deterministically() {
    let no_result = r#"(module
      (memory (export "memory") 1)
      (func (export "run") (param i32 i32) (result i32) i32.const 0))"#;
    assert!(matches!(
        invoke_wat(no_result).await,
        Err(WasmError::InvalidGuestResult(
            InvalidGuestResultKind::AbsentResult
        ))
    ));

    let negative_result = r#"(module
      (memory (export "memory") 1)
      (func (export "run") (param i32 i32) (result i32) i32.const -1))"#;
    assert!(matches!(
        invoke_wat(negative_result).await,
        Err(WasmError::InvalidGuestResult(
            InvalidGuestResultKind::InvalidLength
        ))
    ));

    let payload = br#"{"action":"Done","params":{},"success":true}"#;
    assert!(matches!(
        invoke_wat(&host_result_wat(payload, 2, 0)).await,
        Err(WasmError::InvalidGuestResult(
            InvalidGuestResultKind::MultipleWrites
        ))
    ));
    assert!(matches!(
        invoke_wat(&host_result_wat(payload, 1, RESULT_OFFSET as i32)).await,
        Err(WasmError::InvalidGuestResult(
            InvalidGuestResultKind::MultipleSources
        ))
    ));
    assert!(matches!(
        invoke_wat(&host_result_wat(payload, 1, -1)).await,
        Err(WasmError::InvalidGuestResult(
            InvalidGuestResultKind::InvalidLength
        ))
    ));
}

#[tokio::test]
async fn invalid_utf8_json_and_memory_ranges_use_closed_failure_kinds() {
    assert!(matches!(
        invoke_wat(&host_result_wat(&[0xff], 1, 0)).await,
        Err(WasmError::InvalidGuestResult(
            InvalidGuestResultKind::InvalidUtf8
        ))
    ));
    assert!(matches!(
        invoke_wat(&pointer_result_wat(b"not-json")).await,
        Err(WasmError::InvalidGuestResult(
            InvalidGuestResultKind::InvalidJson
        ))
    ));

    let out_of_bounds = r#"(module
      (import "env" "host_set_result" (func $host_set_result (param i32 i32)))
      (memory (export "memory") 1)
      (func (export "run") (param i32 i32) (result i32)
        i32.const 70000
        i32.const 1
        call $host_set_result
        i32.const 0))"#;
    assert!(matches!(
        invoke_wat(out_of_bounds).await,
        Err(WasmError::InvalidGuestResult(
            InvalidGuestResultKind::OutOfBounds
        ))
    ));
}
