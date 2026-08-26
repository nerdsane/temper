//! Compile the normal typed `temper_module!` authoring path.

use temper_wasm_sdk::prelude::*;

#[unsafe(no_mangle)]
extern "C" fn host_get_context(_buffer_ptr: i32, _buffer_len: i32) -> i32 {
    -1
}

#[unsafe(no_mangle)]
extern "C" fn host_set_result(_result_ptr: i32, _result_len: i32) {}

temper_module! {
    fn typed_failure_module(_ctx: Context) -> TypedModuleResult {
        let failure = GuestFailureDeclarationV1::new(
            FailureCategory::Budget,
            StableFailureCode::new("QuotaExhausted").expect("valid code"),
            FailureRetryability::Never,
            FailureOutcome::NotApplied,
        ).expect("valid declaration");
        Err(failure)
    }
}

#[test]
fn macro_exposes_the_expected_wasm_entrypoint() {
    let _entrypoint: extern "C" fn(i32, i32) -> i32 = run;
}
