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

/// Re-export serde_json types for convenience.
pub use serde_json::{self, Value, json};

/// Set the invocation result as a success callback.
pub fn set_success_result(action: &str, params: &Value) {
    let result = serde_json::json!({
        "action": action,
        "params": params,
        "success": true,
    });
    let json = result.to_string();
    unsafe {
        host::host_set_result(json.as_ptr() as i32, json.len() as i32);
    }
}

/// Set the invocation result as an error.
pub fn set_error_result(error: &str) {
    let result = serde_json::json!({
        "action": "callback",
        "params": { "error": error },
        "success": false,
        "error": error,
    });
    let json = result.to_string();
    unsafe {
        host::host_set_result(json.as_ptr() as i32, json.len() as i32);
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
    pub use crate::{Value, json, serde_json, set_error_result, set_success_result, temper_module};
}
