//! Typed context and host function wrappers for Temper WASM modules.

use core::ptr::addr_of;
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::host;

/// HTTP request sent through the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpRequest {
    /// HTTP method.
    pub method: String,
    /// Absolute request URL.
    pub url: String,
    /// Request headers as ordered key/value pairs.
    pub headers: Vec<(String, String)>,
    /// UTF-8 request body.
    pub body: String,
}

/// HTTP response from a host call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// WASM module name being invoked, when the host provides it.
    pub wasm_module: String,
    /// HTTP dispatch context (present only when this invocation was
    /// routed via an ADR-0069 HttpEndpoint). Guests serving HTTP
    /// unwrap this then drive the inbound exchange via
    /// `temper_wasm_sdk::http_stream::InboundHttp`.
    pub http_request: Option<Value>,
}

pub struct WideEventInput<'a> {
    pub kind: &'a str,
    pub operation: &'a str,
    pub success: bool,
    pub duration_ns: u64,
    pub tags: &'a Value,
    pub attributes: &'a Value,
    pub measurements: &'a Value,
}

/// A structured guest observability span owned by the WASM module.
pub struct WasmSpan {
    id: i64,
    ended: bool,
}

impl WasmSpan {
    /// Return the opaque host span id.
    pub fn id(&self) -> i64 {
        self.id
    }

    /// Add a span event with structured attributes.
    pub fn add_event(&self, name: &str, attributes: &Value) -> Result<(), String> {
        let payload = build_guest_span_event_payload(name, attributes)?;
        call_host_span_event(self.id, &payload)
    }

    /// Add or update span attributes.
    pub fn set_attributes(&self, attributes: &Value) -> Result<(), String> {
        let payload = build_guest_span_attributes_payload(attributes)?;
        call_host_span_attributes(self.id, &payload)
    }

    /// End the span with an OK status.
    pub fn end_ok(mut self, attributes: &Value) -> Result<(), String> {
        self.end_ok_ref(attributes)
    }

    /// End the span with an error status and optional final attributes.
    pub fn end_error(
        mut self,
        error_type: &str,
        error_message: &str,
        attributes: &Value,
    ) -> Result<(), String> {
        self.end_error_ref(error_type, error_message, attributes)
    }

    fn end_ok_ref(&mut self, attributes: &Value) -> Result<(), String> {
        if self.ended {
            return Ok(());
        }
        let payload = build_guest_span_end_ok_payload(attributes)?;
        call_host_span_end(self.id, &payload)?;
        self.ended = true;
        Ok(())
    }

    fn end_error_ref(
        &mut self,
        error_type: &str,
        error_message: &str,
        attributes: &Value,
    ) -> Result<(), String> {
        if self.ended {
            return Ok(());
        }
        let payload = build_guest_span_end_error_payload(error_type, error_message, attributes)?;
        call_host_span_end(self.id, &payload)?;
        self.ended = true;
        Ok(())
    }
}

impl Drop for WasmSpan {
    fn drop(&mut self) {
        if !self.ended {
            let payload = build_guest_span_end_ok_payload(&serde_json::json!({
                "wasm_guest.drop_end": true,
            }))
            .unwrap_or_else(|_| "{\"status\":\"ok\"}".to_string());
            let _ = call_host_span_end(self.id, &payload);
            self.ended = true;
        }
    }
}

impl Context {
    /// Parse the invocation context from the host.
    ///
    /// Reads the context JSON via `host_get_context` and deserializes it.
    pub fn from_host() -> Result<Self, String> {
        let ctx_json = unsafe {
            let ptr = addr_of!(host::CTX_BUF) as *const u8;
            let len = host::host_get_context(ptr as i32, host::CTX_BUF_LEN as i32);
            if len <= 0 {
                return Err("failed to read invocation context".to_string());
            }

            if len as usize <= host::CTX_BUF_LEN {
                let slice = core::slice::from_raw_parts(ptr, len as usize);
                String::from_utf8_lossy(slice).to_string()
            } else {
                let needed = len as usize;
                if needed > i32::MAX as usize {
                    return Err("failed to read invocation context".to_string());
                }

                let mut buf = vec![0u8; needed];
                let actual = host::host_get_context(buf.as_mut_ptr() as i32, needed as i32);
                if actual <= 0 || actual as usize > buf.len() {
                    return Err("failed to read invocation context".to_string());
                }

                let slice = core::slice::from_raw_parts(buf.as_ptr(), actual as usize);
                String::from_utf8_lossy(slice).to_string()
            }
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
        let wasm_module = parsed
            .get("wasm_module")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        let http_request = parsed.get("http_request").cloned();

        Ok(Self {
            config,
            trigger_params,
            entity_state,
            tenant,
            entity_type,
            entity_id,
            trigger_action,
            wasm_module,
            http_request,
        })
    }

    /// Get current UTC time as milliseconds since Unix epoch.
    ///
    /// Useful for elapsed-time tracking in WASM modules where
    /// `std::time::Instant` is not available.
    pub fn get_time_millis() -> i64 {
        unsafe { host::host_get_time_millis() }
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
            if len < 0 {
                return Err(format!("HTTP call failed with code {len}: {method} {url}"));
            }
            // len == 0 is a legitimate empty payload, not an error.
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

    /// Make multiple HTTP requests via the host and collect the responses in order.
    pub fn http_call_batch(&self, requests: &[HttpRequest]) -> Result<Vec<HttpResponse>, String> {
        if requests.is_empty() {
            return Ok(Vec::new());
        }

        let requests_json = serde_json::to_string(requests)
            .map_err(|e| format!("failed to serialize batch HTTP requests: {e}"))?;

        let response_json = unsafe {
            let ptr = addr_of!(host::HTTP_BUF) as *const u8;
            let len = host::host_http_call_batch(
                requests_json.as_ptr() as i32,
                requests_json.len() as i32,
                ptr as i32,
                host::HTTP_BUF_LEN as i32,
            );
            if len == -1 {
                return Err("HTTP batch call failed".to_string());
            }
            if len == -2 {
                return Err("HTTP batch response too large for buffer".to_string());
            }
            if len < 0 {
                return Err(format!("HTTP batch call failed with code {len}"));
            }
            if len == 0 {
                // Legitimate empty payload (no responses), not an error.
                return Ok(Vec::new());
            }
            let slice = core::slice::from_raw_parts(ptr, len as usize);
            String::from_utf8_lossy(slice).to_string()
        };

        serde_json::from_str(&response_json)
            .map_err(|e| format!("failed to parse batch HTTP responses: {e}"))
    }

    /// Make a Connect protocol server-streaming RPC call via the host.
    ///
    /// Sends a POST request with JSON body using the Connect protocol.
    /// The host handles binary frame parsing and returns decoded JSON payloads.
    /// Returns a vec of JSON strings, one per data frame in the response.
    pub fn connect_call(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &str,
    ) -> Result<Vec<String>, String> {
        let headers_json = if headers.is_empty() {
            String::new()
        } else {
            serde_json::to_string(headers).unwrap_or_default()
        };

        let response = unsafe {
            let ptr = addr_of!(host::HTTP_BUF) as *const u8;
            let len = host::host_connect_call(
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
                return Err(format!("Connect call failed: {url}"));
            }
            if len == -2 {
                return Err("Connect response too large for buffer".to_string());
            }
            if len < 0 {
                return Err(format!("Connect call failed with code {len}: {url}"));
            }
            if len == 0 {
                // Legitimate empty payload (no frames), not an error.
                return Ok(Vec::new());
            }
            let slice = core::slice::from_raw_parts(ptr, len as usize);
            String::from_utf8_lossy(slice).to_string()
        };

        serde_json::from_str(&response)
            .map_err(|e| format!("failed to parse Connect response frames: {e}"))
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
            // The host returns the secret's full length even when it exceeds
            // the buffer (nothing was written in that case). Reading `len`
            // bytes unconditionally would over-read the fixed SECRET_BUF.
            if len as usize > host::SECRET_BUF_LEN {
                return Err(format!(
                    "secret '{key}' is {len} bytes, exceeds buffer of {}",
                    host::SECRET_BUF_LEN
                ));
            }
            let slice = core::slice::from_raw_parts(ptr, len as usize);
            Ok(String::from_utf8_lossy(slice).to_string())
        }
    }

    /// Emit a replayable progress event for the current entity.
    pub fn emit_progress(&self, event: &Value) -> Result<(), String> {
        let json =
            serde_json::to_string(event).map_err(|e| format!("progress JSON serialize: {e}"))?;
        let rc = unsafe { host::host_emit_progress(json.as_ptr() as i32, json.len() as i32) };
        if rc == 0 {
            Ok(())
        } else {
            Err("host_emit_progress failed".to_string())
        }
    }

    /// Emit a wide event from the guest module.
    pub fn emit_wide_event(&self, event: &WideEventInput<'_>) -> Result<(), String> {
        let json = serde_json::json!({
            "kind": event.kind,
            "operation": event.operation,
            "success": event.success,
            "duration_ns": event.duration_ns,
            "tags": event.tags,
            "attributes": event.attributes,
            "measurements": event.measurements,
        })
        .to_string();
        let rc = unsafe { host::host_emit_wide_event(json.as_ptr() as i32, json.len() as i32) };
        if rc == 0 {
            Ok(())
        } else {
            Err("host_emit_wide_event failed".to_string())
        }
    }

    /// Emit a structured log event from the guest module.
    pub fn log_structured(&self, level: &str, message: &str, fields: &Value) -> Result<(), String> {
        let json = serde_json::json!({
            "level": level,
            "message": message,
            "fields": fields,
        })
        .to_string();
        let rc = unsafe { host::host_log_structured(json.as_ptr() as i32, json.len() as i32) };
        if rc == 0 {
            Ok(())
        } else {
            Err("host_log_structured failed".to_string())
        }
    }

    /// Emit a metric directly from the guest module.
    pub fn emit_metric(
        &self,
        name: &str,
        value: f64,
        tags: &Value,
        kind: Option<&str>,
    ) -> Result<(), String> {
        let json = serde_json::json!({
            "name": name,
            "value": value,
            "tags": tags,
            "kind": kind,
        })
        .to_string();
        let rc = unsafe { host::host_emit_metric(json.as_ptr() as i32, json.len() as i32) };
        if rc == 0 {
            Ok(())
        } else {
            Err("host_emit_metric failed".to_string())
        }
    }

    /// Start a structured guest observability span.
    pub fn start_span(&self, name: &str, attributes: &Value) -> Result<WasmSpan, String> {
        self.start_span_with_kind(name, None, attributes)
    }

    /// Start a structured guest observability span with an explicit span kind.
    pub fn start_span_with_kind(
        &self,
        name: &str,
        kind: Option<&str>,
        attributes: &Value,
    ) -> Result<WasmSpan, String> {
        let payload = build_guest_span_start_payload(name, kind, attributes)?;
        let span_id =
            unsafe { host::host_start_span(payload.as_ptr() as i32, payload.len() as i32) };
        if span_id <= 0 {
            return Err(format!("host_start_span failed for '{name}'"));
        }
        Ok(WasmSpan {
            id: span_id,
            ended: false,
        })
    }

    /// Evaluate a single transition against an IOA spec via the host.
    ///
    /// The host builds a `TransitionTable` from the IOA source and evaluates
    /// the given action from the given state. Returns parsed JSON result with
    /// `success`, `new_state`, `error`, and `guard_result` fields.
    pub fn evaluate_spec(
        &self,
        ioa_source: &str,
        current_state: &str,
        action: &str,
        params_json: &str,
    ) -> Result<Value, String> {
        let response = unsafe {
            let ptr = addr_of!(host::SPEC_EVAL_BUF) as *const u8;
            let len = host::host_evaluate_spec(
                ioa_source.as_ptr() as i32,
                ioa_source.len() as i32,
                current_state.as_ptr() as i32,
                current_state.len() as i32,
                action.as_ptr() as i32,
                action.len() as i32,
                params_json.as_ptr() as i32,
                params_json.len() as i32,
                ptr as i32,
                host::SPEC_EVAL_BUF_LEN as i32,
            );
            if len == -1 {
                return Err("evaluate_spec call failed".to_string());
            }
            if len == -2 {
                return Err("evaluate_spec response too large for buffer".to_string());
            }
            if len < 0 {
                return Err(format!("evaluate_spec failed with code {len}"));
            }
            if len == 0 {
                // The host always writes a JSON object on success, so an
                // empty response is an error, not an empty success.
                return Err("evaluate_spec returned empty response".to_string());
            }
            let slice = core::slice::from_raw_parts(ptr, len as usize);
            String::from_utf8_lossy(slice).to_string()
        };

        serde_json::from_str(&response)
            .map_err(|e| format!("failed to parse evaluate_spec response: {e}"))
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

    /// Read an entity-state field as raw bytes, transparently resolving
    /// blob-ref envelopes via the host. Works for both inline values and
    /// oversize blob-ref fields (populated by the dispatcher's blob prefetch).
    ///
    /// For plain string fields the return bytes are the UTF-8 text (unquoted);
    /// for blob-ref fields the return bytes are the original decoded payload;
    /// for other JSON values the return bytes are the JSON serialization.
    ///
    /// See ADR-0046.
    pub fn read_field_bytes(&self, field_name: &str) -> Result<Vec<u8>, String> {
        // Probe with a zero-length buffer to get the required size.
        let needed = unsafe {
            host::host_read_field(field_name.as_ptr() as i32, field_name.len() as i32, 0, 0)
        };
        if needed == -1 {
            return Err(format!("field '{field_name}' not found"));
        }
        if needed == -2 {
            return Err(format!(
                "field '{field_name}' is a blob ref; pre-fetch missing"
            ));
        }
        if needed == -3 {
            return Err(format!("host_read_field error for '{field_name}'"));
        }
        if needed == 0 {
            return Ok(Vec::new());
        }
        let mut buf = vec![0u8; needed as usize];
        let written = unsafe {
            host::host_read_field(
                field_name.as_ptr() as i32,
                field_name.len() as i32,
                buf.as_mut_ptr() as i32,
                buf.len() as i32,
            )
        };
        if written < 0 || (written as usize) > buf.len() {
            return Err(format!(
                "host_read_field second call failed for '{field_name}': {written}"
            ));
        }
        buf.truncate(written as usize);
        Ok(buf)
    }

    /// Read an entity-state field as a UTF-8 string. Convenience wrapper
    /// over [`read_field_bytes`] for the common string-field case.
    pub fn read_field_string(&self, field_name: &str) -> Result<String, String> {
        let bytes = self.read_field_bytes(field_name)?;
        String::from_utf8(bytes)
            .map_err(|e| format!("field '{field_name}' is not valid UTF-8: {e}"))
    }
}

fn object_attributes(attributes: &Value) -> Value {
    match attributes {
        Value::Object(_) => attributes.clone(),
        _ => serde_json::json!({}),
    }
}

fn build_guest_span_start_payload(
    name: &str,
    kind: Option<&str>,
    attributes: &Value,
) -> Result<String, String> {
    let mut payload = serde_json::json!({
        "name": name,
        "attributes": object_attributes(attributes),
    });
    if let Some(kind) = kind.filter(|value| !value.is_empty()) {
        payload["kind"] = Value::String(kind.to_string());
    }
    serde_json::to_string(&payload).map_err(|e| format!("span start JSON serialize: {e}"))
}

fn build_guest_span_event_payload(name: &str, attributes: &Value) -> Result<String, String> {
    let payload = serde_json::json!({
        "name": name,
        "attributes": object_attributes(attributes),
    });
    serde_json::to_string(&payload).map_err(|e| format!("span event JSON serialize: {e}"))
}

fn build_guest_span_attributes_payload(attributes: &Value) -> Result<String, String> {
    let payload = serde_json::json!({
        "attributes": object_attributes(attributes),
    });
    serde_json::to_string(&payload).map_err(|e| format!("span attributes JSON serialize: {e}"))
}

fn build_guest_span_end_ok_payload(attributes: &Value) -> Result<String, String> {
    let payload = serde_json::json!({
        "status": "ok",
        "attributes": object_attributes(attributes),
    });
    serde_json::to_string(&payload).map_err(|e| format!("span end JSON serialize: {e}"))
}

fn build_guest_span_end_error_payload(
    error_type: &str,
    error_message: &str,
    attributes: &Value,
) -> Result<String, String> {
    let payload = serde_json::json!({
        "status": "error",
        "error.type": error_type,
        "error.message": error_message,
        "attributes": object_attributes(attributes),
    });
    serde_json::to_string(&payload).map_err(|e| format!("span error JSON serialize: {e}"))
}

fn call_host_span_event(span_id: i64, payload: &str) -> Result<(), String> {
    let rc = unsafe {
        host::host_add_span_event(span_id, payload.as_ptr() as i32, payload.len() as i32)
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(format!("host_add_span_event failed for span {span_id}"))
    }
}

fn call_host_span_attributes(span_id: i64, payload: &str) -> Result<(), String> {
    let rc = unsafe {
        host::host_set_span_attributes(span_id, payload.as_ptr() as i32, payload.len() as i32)
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(format!(
            "host_set_span_attributes failed for span {span_id}"
        ))
    }
}

fn call_host_span_end(span_id: i64, payload: &str) -> Result<(), String> {
    let rc = unsafe { host::host_end_span(span_id, payload.as_ptr() as i32, payload.len() as i32) };
    if rc == 0 {
        Ok(())
    } else {
        Err(format!("host_end_span failed for span {span_id}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_guest_span_lifecycle_payloads() {
        let start = build_guest_span_start_payload(
            "tool.llm_call",
            Some("client"),
            &serde_json::json!({
                "tool.name": "provider_caller",
                "gen_ai.request.model": "claude",
            }),
        )
        .expect("start payload");
        let start: Value = serde_json::from_str(&start).expect("valid start JSON");
        assert_eq!(start["name"], "tool.llm_call");
        assert_eq!(start["kind"], "client");
        assert_eq!(start["attributes"]["tool.name"], "provider_caller");

        let event =
            build_guest_span_event_payload("llm.response", &serde_json::json!({"tokens": 42}))
                .expect("event payload");
        let event: Value = serde_json::from_str(&event).expect("valid event JSON");
        assert_eq!(event["name"], "llm.response");
        assert_eq!(event["attributes"]["tokens"], 42);

        let attrs = build_guest_span_attributes_payload(&serde_json::json!({"completion": "stop"}))
            .expect("attrs payload");
        let attrs: Value = serde_json::from_str(&attrs).expect("valid attrs JSON");
        assert_eq!(attrs["attributes"]["completion"], "stop");

        let end = build_guest_span_end_error_payload(
            "llm_error",
            "provider failed",
            &serde_json::json!({"http.status_code": 500}),
        )
        .expect("end payload");
        let end: Value = serde_json::from_str(&end).expect("valid end JSON");
        assert_eq!(end["status"], "error");
        assert_eq!(end["error.type"], "llm_error");
        assert_eq!(end["error.message"], "provider failed");
    }

    #[test]
    fn non_object_span_attributes_become_empty_objects() {
        let payload =
            build_guest_span_start_payload("guest", None, &Value::String("ignored".to_string()))
                .expect("payload");
        let payload: Value = serde_json::from_str(&payload).expect("valid JSON");
        assert_eq!(payload["attributes"], serde_json::json!({}));
    }
}
