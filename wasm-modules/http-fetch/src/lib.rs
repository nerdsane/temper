//! Generic HTTP fetch WASM module for Temper integrations.
//!
//! Reads URL, method, and headers from `integration_config` in the invocation
//! context, appends `trigger_params` as query parameters (GET) or JSON body
//! (POST), calls the host HTTP function, and returns the response via callback.
//!
//! Build: `cargo build -p http-fetch-module --target wasm32-unknown-unknown --release`

use temper_wasm_sdk::prelude::*;

temper_module! {
    fn run(ctx: Context) -> Result<Value> {
        ctx.log("info", "http-fetch: run() called");

        let url = ctx.config.get("url")
            .ok_or("integration_config missing 'url' key")?
            .clone();

        let method = ctx.config.get("method")
            .cloned()
            .unwrap_or_else(|| "GET".to_string());

        let headers_str = ctx.config.get("headers")
            .cloned()
            .unwrap_or_default();

        // Parse headers JSON string into Vec<(String, String)> if present.
        let headers: Vec<(String, String)> = if headers_str.is_empty() {
            vec![]
        } else {
            serde_json::from_str(&headers_str).unwrap_or_default()
        };

        let trigger_json = ctx.trigger_params.to_string();

        // Build final URL/body depending on method.
        let (final_url, body) = if method.eq_ignore_ascii_case("GET") {
            let qs = params_to_query_string(&ctx.trigger_params);
            if qs.is_empty() {
                (url, String::new())
            } else {
                let sep = if url.contains('?') { "&" } else { "?" };
                (format!("{url}{sep}{qs}"), String::new())
            }
        } else {
            (url, trigger_json)
        };

        ctx.log("info", &format!("http-fetch: {method} {final_url}"));

        let resp = ctx.http_call(&method, &final_url, &headers, &body)?;

        ctx.log("info", &format!("http-fetch: status={} body_len={}", resp.status, resp.body.len()));

        // Parse response body as JSON if possible, otherwise return as string.
        let response_value: Value = serde_json::from_str(&resp.body)
            .unwrap_or_else(|_| Value::String(resp.body));

        Ok(json!({
            "status_code": resp.status,
            "response": response_value,
        }))
    }
}

/// Convert a JSON object `{"key":"val",...}` to query string `key=val&...`.
fn params_to_query_string(params: &Value) -> String {
    let Some(obj) = params.as_object() else {
        return String::new();
    };
    if obj.is_empty() {
        return String::new();
    }
    obj.iter()
        .map(|(k, v)| {
            let val = match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            format!("{k}={val}")
        })
        .collect::<Vec<_>>()
        .join("&")
}
