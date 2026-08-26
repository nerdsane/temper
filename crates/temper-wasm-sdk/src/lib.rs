//! SDK for writing Temper WASM integration modules.
//!
//! Provides a typed, ergonomic API over the raw WASM host function ABI.
//! Module authors use the `temper_module!` macro to define their entry point
//! and the `Context` struct to interact with the host.
//!
//! # Example
//!
//! ```ignore
//! use temper_wasm_sdk::prelude::*;
//!
//! temper_module! {
//!     fn run(ctx: Context) -> Result<Value> {
//!         let resp = ctx.http_get(&ctx.config["url"])?;
//!         let data: Value = serde_json::from_str(&resp.body)?;
//!         Ok(json!({ "temperature": data["current"]["temperature_2m"] }))
//!     }
//! }
//! ```

pub mod context;
pub mod data;
pub mod host;
pub mod schema_deployment;

#[cfg(target_arch = "wasm32")]
pub mod http_stream;

#[cfg(not(target_arch = "wasm32"))]
pub mod http_stream {
    /// One end of a streaming channel owned by the host.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct StreamHandle(pub u32);

    /// Errors surfaced by the streaming wrappers.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum StreamError {
        Closed,
        InvalidHandle,
        Other(String),
    }

    impl core::fmt::Display for StreamError {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            match self {
                StreamError::Closed => write!(f, "stream closed"),
                StreamError::InvalidHandle => write!(f, "invalid stream handle"),
                StreamError::Other(msg) => write!(f, "stream error: {msg}"),
            }
        }
    }

    /// Host-build placeholder for the wasm32 request-body writer.
    pub struct HttpRequestBodyWriter;

    impl HttpRequestBodyWriter {
        pub fn handle(&self) -> StreamHandle {
            StreamHandle(0)
        }

        pub fn write_all_chunk(&mut self, _chunk: &[u8]) -> Result<usize, StreamError> {
            Err(StreamError::Other(
                "http streaming host functions are only available on wasm32".to_string(),
            ))
        }

        pub fn finish(self) -> Result<(), StreamError> {
            Err(StreamError::Other(
                "http streaming host functions are only available on wasm32".to_string(),
            ))
        }
    }

    /// Host-build placeholder for the wasm32 response-body reader.
    pub struct HttpResponseBodyReader;

    impl HttpResponseBodyReader {
        pub fn handle(&self) -> StreamHandle {
            StreamHandle(0)
        }

        pub fn read_next_chunk(&mut self, _buf: &mut [u8]) -> Result<Option<usize>, StreamError> {
            Err(StreamError::Other(
                "http streaming host functions are only available on wasm32".to_string(),
            ))
        }

        pub fn close(self) -> Result<(), StreamError> {
            Err(StreamError::Other(
                "http streaming host functions are only available on wasm32".to_string(),
            ))
        }
    }

    /// Response head handed to the guest once the host has parsed the HTTP response.
    #[derive(Debug, Clone, Default)]
    pub struct HttpResponseHead {
        pub status: u16,
        pub headers: Vec<(String, String)>,
    }

    pub type ResponseHeadFetcher = fn() -> Result<HttpResponseHead, StreamError>;
    pub type StreamingCallParts = (
        HttpRequestBodyWriter,
        HttpResponseBodyReader,
        ResponseHeadFetcher,
    );

    /// Inbound HTTP dispatch context delivered through `WasmInvocationContext.http_request`.
    #[derive(Debug, Clone, serde::Deserialize)]
    pub struct InboundHttp {
        pub method: String,
        pub path: String,
        #[serde(default)]
        pub params: std::collections::BTreeMap<String, String>,
        #[serde(default)]
        pub headers: Vec<(String, String)>,
        #[serde(default)]
        pub principal_id: Option<String>,
        pub request_body_handle: u32,
        pub response_body_handle: u32,
    }

    impl InboundHttp {
        pub fn request_body(&self) -> HttpResponseBodyReader {
            HttpResponseBodyReader
        }

        pub fn response_body(&self) -> HttpRequestBodyWriter {
            HttpRequestBodyWriter
        }

        pub fn submit_response_head(
            &self,
            _status: u16,
            _headers: &[(&str, &str)],
        ) -> Result<(), StreamError> {
            Err(StreamError::Other(
                "http streaming host functions are only available on wasm32".to_string(),
            ))
        }
    }

    pub fn streaming_call(
        _method: &str,
        _url: &str,
        _headers: &[(&str, &str)],
    ) -> Result<StreamingCallParts, StreamError> {
        Err(StreamError::Other(
            "http streaming host functions are only available on wasm32".to_string(),
        ))
    }
}

pub use context::{Context, HttpRequest, HttpResponse, SubWrite, SubWriteBuilder, WasmSpan};
pub use temper_failure::{
    BoundedDetailString, BoundedDiagnostic, BoundedFailureDetails, DetailKey, FailureCategory,
    FailureContractError, FailureDetailValue, FailureOutcome, FailureRetryability,
    GuestFailureDeclarationV1, StableFailureCode,
};

/// Re-export serde_json types for convenience.
pub use serde_json::{self, Value, json};

/// Typed result returned by the normal module macro authoring path.
pub type TypedModuleResult = Result<Value, GuestFailureDeclarationV1>;

/// Convert invocation-context decoding failure into a bounded typed failure.
#[doc(hidden)]
pub fn invalid_invocation_context_failure(error: String) -> GuestFailureDeclarationV1 {
    let failure = GuestFailureDeclarationV1::new(
        FailureCategory::Integrity,
        StableFailureCode::new("InvalidInvocationContext")
            .expect("static failure code satisfies the contract"),
        FailureRetryability::Never,
        FailureOutcome::NotApplied,
    )
    .expect("static failure declaration satisfies the contract");
    match failure.clone().with_diagnostic(error) {
        Ok(with_diagnostic) => with_diagnostic,
        Err(_) => failure,
    }
}

fn write_result_json(json: &str) {
    unsafe {
        host::host_set_result(json.as_ptr() as i32, json.len() as i32);
    }
}

fn encode_success_result(action: &str, params: &Value) -> String {
    serde_json::json!({
        "action": action,
        "params": params,
        "success": true,
    })
    .to_string()
}

fn encode_error_result(error: &str) -> String {
    #[derive(serde::Serialize)]
    struct LegacyErrorParams<'a> {
        error: &'a str,
    }

    #[derive(serde::Serialize)]
    struct LegacyTerminalResult<'a> {
        action: &'static str,
        params: LegacyErrorParams<'a>,
        success: bool,
        error: &'a str,
    }

    serde_json::to_string(&LegacyTerminalResult {
        action: "callback",
        params: LegacyErrorParams { error },
        success: false,
        error,
    })
    .expect("serializing a legacy terminal result with string fields cannot fail")
}

fn encode_typed_failure_result(
    failure: &GuestFailureDeclarationV1,
) -> Result<String, serde_json::Error> {
    #[derive(serde::Serialize)]
    struct TypedTerminalResult<'a> {
        success: bool,
        typed_failure: &'a GuestFailureDeclarationV1,
    }

    serde_json::to_string(&TypedTerminalResult {
        success: false,
        typed_failure: failure,
    })
}

/// Set the invocation result as a success callback.
pub fn set_success_result(action: &str, params: &Value) {
    write_result_json(&encode_success_result(action, params));
}

/// Set the invocation result as an error.
pub fn set_error_result(error: &str) {
    write_result_json(&encode_error_result(error));
}

/// Set a bounded typed terminal-failure declaration.
///
/// Serialization revalidates public declaration fields. If a caller mutates a
/// declaration into an invalid state, this writes a deliberately invalid typed
/// shape so the kernel produces `InvalidGuestFailureResult` rather than
/// accepting or reclassifying it.
pub fn set_typed_failure_result(failure: &GuestFailureDeclarationV1) {
    match encode_typed_failure_result(failure) {
        Ok(json) => write_result_json(&json),
        Err(_) => write_result_json(r#"{"success":false,"typed_failure":null}"#),
    }
}

/// Macro to define a Temper WASM module entry point.
///
/// Generates the `extern "C" fn run` with proper ABI, context parsing,
/// and result handling. The user function receives a `Context` and returns
/// `Result<Value, String>`.
///
/// The returned `Value` should be the callback params. The macro wraps it
/// in the standard `{"action":"callback","params":...,"success":true}` format.
///
/// # Example
///
/// ```ignore
/// temper_module! {
///     fn run(ctx: Context) -> Result<Value> {
///         ctx.log("info", "module executing");
///         let resp = ctx.http_get(&ctx.config["url"])?;
///         Ok(serde_json::from_str(&resp.body)?)
///     }
/// }
/// ```
#[macro_export]
macro_rules! temper_module {
    (fn $name:ident($ctx:ident : Context) -> TypedModuleResult $body:block) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn run(_ctx_ptr: i32, _ctx_len: i32) -> i32 {
            let result = (|| -> $crate::TypedModuleResult {
                let $ctx = $crate::Context::from_host().map_err(|error| {
                    $crate::invalid_invocation_context_failure(error.to_string())
                })?;
                $body
            })();

            match result {
                Ok(value) => $crate::set_success_result("callback", &value),
                Err(failure) => $crate::set_typed_failure_result(&failure),
            }
            0
        }
    };
    (fn $name:ident($ctx:ident : Context) -> Result<Value> $body:block) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn run(_ctx_ptr: i32, _ctx_len: i32) -> i32 {
            let result = (|| -> Result<$crate::Value, String> {
                let $ctx = $crate::Context::from_host().map_err(|e| e.to_string())?;
                $body
            })();

            match result {
                Ok(val) => {
                    $crate::set_success_result("callback", &val);
                }
                Err(e) => {
                    $crate::set_error_result(&e);
                }
            }
            0
        }
    };
}

/// Prelude module for convenient imports.
///
/// ```ignore
/// use temper_wasm_sdk::prelude::*;
/// ```
pub mod prelude {
    pub use crate::context::{
        Context, HttpRequest, HttpResponse, SubWrite, SubWriteBuilder, WasmSpan,
    };
    pub use crate::data::{DataClient, ModuleDataError};
    pub use crate::{
        BoundedDetailString, BoundedDiagnostic, BoundedFailureDetails, DetailKey, FailureCategory,
        FailureContractError, FailureDetailValue, FailureOutcome, FailureRetryability,
        GuestFailureDeclarationV1, StableFailureCode, TypedModuleResult, Value, json, serde_json,
        set_error_result, set_success_result, set_typed_failure_result, temper_module,
    };
}

#[cfg(test)]
mod terminal_result_tests {
    use super::*;

    #[test]
    fn existing_success_and_legacy_error_bytes_are_unchanged() {
        assert_eq!(
            encode_success_result("ChargeSucceeded", &json!({"provider_id": "p-1"})),
            r#"{"action":"ChargeSucceeded","params":{"provider_id":"p-1"},"success":true}"#
        );
        assert_eq!(
            encode_success_result("", &json!({"stored": true})),
            r#"{"action":"","params":{"stored":true},"success":true}"#
        );
        assert_eq!(
            encode_error_result("provider rejected request"),
            r#"{"action":"callback","params":{"error":"provider rejected request"},"success":false,"error":"provider rejected request"}"#
        );
    }

    #[test]
    fn typed_failure_bytes_are_exact_and_exclude_kernel_fields() {
        let mut failure = GuestFailureDeclarationV1::new(
            FailureCategory::Transient,
            StableFailureCode::new("ProviderUnavailable").expect("valid code"),
            FailureRetryability::WithBackoff,
            FailureOutcome::NotApplied,
        )
        .expect("valid declaration")
        .with_diagnostic("provider did not accept the request")
        .expect("bounded diagnostic");
        failure
            .try_insert_detail(
                DetailKey::new("status").expect("valid key"),
                FailureDetailValue::Unsigned(503),
            )
            .expect("bounded details");

        assert_eq!(
            encode_typed_failure_result(&failure).expect("encode typed failure"),
            r#"{"success":false,"typed_failure":{"version":1,"category":"transient","code":"ProviderUnavailable","retryability":"with_backoff","outcome":"not_applied","diagnostic":"provider did not accept the request","details":{"status":{"kind":"unsigned","value":503}}}}"#
        );
    }

    #[test]
    fn invalid_mutated_declarations_do_not_serialize() {
        let mut failure = GuestFailureDeclarationV1::new(
            FailureCategory::Permanent,
            StableFailureCode::new("ProviderRejected").expect("valid code"),
            FailureRetryability::Never,
            FailureOutcome::NotApplied,
        )
        .expect("valid declaration");
        failure.version = 2;
        assert!(encode_typed_failure_result(&failure).is_err());
    }
}
