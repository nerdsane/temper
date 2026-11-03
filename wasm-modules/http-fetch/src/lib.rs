//! Generic HTTP fetch WASM module for Temper integrations.
//!
//! Reads URL, method, and headers from the context's top-level
//! `integration_config` (the spec's own integration config), appends
//! `trigger_params` as query parameters (GET) or JSON body (POST), calls the
//! host HTTP function, and returns the response via callback. Action params and
//! entity fields are request data only; they never choose where the request
//! goes.
//!
//! Build: `scripts/build-http-fetch-wasm.sh` rebuilds this module and copies it to
//! `crates/temper-wasm/modules/http_fetch.wasm`, the binary the server embeds.

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

        let headers = parse_headers(&headers_str)?;

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

/// The configured `headers` value: a JSON object (`{"Name": "value"}`) or a
/// JSON array of `[name, value]` pairs. Empty means no headers. A value that is
/// neither is an error, so a misconfigured integration fails visibly instead of
/// sending its request without the headers it declared.
fn parse_headers(raw: &str) -> Result<Vec<(String, String)>, String> {
    if raw.trim().is_empty() {
        return Ok(vec![]);
    }
    let value: Value = serde_json::from_str(raw)
        .map_err(|e| format!("integration_config 'headers' is not JSON: {e}"))?;
    match value {
        Value::Object(map) => map
            .into_iter()
            .map(|(name, value)| match value {
                Value::String(value) => Ok((name, value)),
                other => Err(format!("header '{name}' must be a string, got {other}")),
            })
            .collect(),
        Value::Array(_) => serde_json::from_value(value).map_err(|e| {
            format!("integration_config 'headers' must be [name, value] pairs: {e}")
        }),
        other => Err(format!(
            "integration_config 'headers' must be an object or a list of pairs, got {other}"
        )),
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
