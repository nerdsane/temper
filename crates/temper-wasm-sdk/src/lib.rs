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
pub mod host;

pub use context::{Context, HttpResponse};

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
    pub use crate::context::{Context, HttpResponse};
    pub use crate::{Value, json, serde_json, set_error_result, set_success_result, temper_module};
}
