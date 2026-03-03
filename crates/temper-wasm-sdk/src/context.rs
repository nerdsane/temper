//! Typed context and host function wrappers for Temper WASM modules.

use core::ptr::addr_of;
use std::collections::BTreeMap;

use serde_json::Value;

use crate::host;

/// HTTP response from a host call.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    /// HTTP status code.
    pub status: u16,
    /// Response body as a string.
    pub body: String,
}

/// Typed invocation context for a Temper WASM module.
///
/// Provides access to integration config, trigger parameters, entity state,
/// and typed wrappers for host functions (HTTP, secrets, logging).
pub struct Context {
    /// Configuration from the `[[integration]]` section.
    pub config: BTreeMap<String, String>,
    /// Parameters from the triggering action.
    pub trigger_params: Value,
    /// Current entity state snapshot.
    pub entity_state: Value,
    /// Tenant ID.
    pub tenant: String,
    /// Entity type.
    pub entity_type: String,
    /// Entity instance ID.
    pub entity_id: String,
    /// The action that triggered this integration.
    pub trigger_action: String,
}

impl Context {
    /// Parse the invocation context from the host.
    ///
    /// Reads the context JSON via `host_get_context` and deserializes it.
    pub fn from_host() -> Result<Self, String> {
        let ctx_json = unsafe {
            let ptr = addr_of!(host::CTX_BUF) as *const u8;
            let len = host::host_get_context(ptr as i32, host::CTX_BUF_LEN as i32);
            if len <= 0 || len as usize > host::CTX_BUF_LEN {
                return Err("failed to read invocation context".to_string());
            }
            let slice = core::slice::from_raw_parts(ptr, len as usize);
            String::from_utf8_lossy(slice).to_string()
        };

        let parsed: Value = serde_json::from_str(&ctx_json)
            .map_err(|e| format!("failed to parse context JSON: {e}"))?;

        let config: BTreeMap<String, String> = parsed
            .get("integration_config")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();

        let trigger_params = parsed
            .get("trigger_params")
            .cloned()
            .unwrap_or(Value::Object(Default::default()));

        let entity_state = parsed
            .get("entity_state")
            .cloned()
            .unwrap_or(Value::Object(Default::default()));

        let tenant = parsed
            .get("tenant")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        let entity_type = parsed
            .get("entity_type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        let entity_id = parsed
            .get("entity_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        let trigger_action = parsed
            .get("trigger_action")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        Ok(Self {
            config,
            trigger_params,
            entity_state,
            tenant,
            entity_type,
            entity_id,
            trigger_action,
        })
    }

    /// Make an HTTP GET request via the host.
    pub fn http_get(&self, url: &str) -> Result<HttpResponse, String> {
        self.http_call("GET", url, &[], "")
    }

    /// Make an HTTP POST request via the host.
    pub fn http_post(&self, url: &str, body: &str) -> Result<HttpResponse, String> {
        self.http_call("POST", url, &[], body)
    }

    /// Make an HTTP request with full control over method, headers, and body.
    pub fn http_call(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: &str,
    ) -> Result<HttpResponse, String> {
        let headers_json = if headers.is_empty() {
            String::new()
        } else {
            serde_json::to_string(headers).unwrap_or_default()
        };

        let response = unsafe {
            let ptr = addr_of!(host::HTTP_BUF) as *const u8;
            let len = host::host_http_call(
                method.as_ptr() as i32,
                method.len() as i32,
                url.as_ptr() as i32,
                url.len() as i32,
                headers_json.as_ptr() as i32,
                headers_json.len() as i32,
                body.as_ptr() as i32,
                body.len() as i32,
                ptr as i32,
                host::HTTP_BUF_LEN as i32,
            );
            if len == -1 {
                return Err(format!("HTTP call failed: {method} {url}"));
            }
            if len == -2 {
                return Err("HTTP response too large for buffer".to_string());
            }
            if len <= 0 {
                return Err("HTTP call returned empty response".to_string());
            }
            let slice = core::slice::from_raw_parts(ptr, len as usize);
            String::from_utf8_lossy(slice).to_string()
        };

        // Parse "status_code\nbody" format
        let (status, resp_body) = match response.find('\n') {
            Some(pos) => {
                let code_str = &response[..pos];
                let body = &response[pos + 1..];
                let code = code_str.parse::<u16>().unwrap_or(0);
                (code, body.to_string())
            }
            None => (0, response),
        };

        Ok(HttpResponse {
            status,
            body: resp_body,
        })
    }

    /// Read a secret value by key from the host.
    pub fn get_secret(&self, key: &str) -> Result<String, String> {
        unsafe {
            let ptr = addr_of!(host::SECRET_BUF) as *const u8;
            let len = host::host_get_secret(
                key.as_ptr() as i32,
                key.len() as i32,
                ptr as i32,
                host::SECRET_BUF_LEN as i32,
            );
            if len < 0 {
                return Err(format!("failed to read secret '{key}'"));
            }
            let slice = core::slice::from_raw_parts(ptr, len as usize);
            Ok(String::from_utf8_lossy(slice).to_string())
        }
    }

    /// Log a message via the host.
    pub fn log(&self, level: &str, msg: &str) {
        unsafe {
            host::host_log(
                level.as_ptr() as i32,
                level.len() as i32,
                msg.as_ptr() as i32,
                msg.len() as i32,
            );
        }
    }
}
