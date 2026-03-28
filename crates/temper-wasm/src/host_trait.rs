//! DST-compliant host function trait and implementations.
//!
//! Production uses real HTTP + secret store. Simulation uses canned
//! responses for deterministic testing.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;

/// Host capabilities provided to WASM modules.
///
/// Production uses real HTTP + secret store. Simulation uses canned
/// responses for deterministic testing.
#[async_trait]
pub trait WasmHost: Send + Sync {
    /// Make an HTTP request. Returns (status_code, response_body).
    async fn http_call(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: &str,
    ) -> Result<(u16, String), String>;

    /// Retrieve a secret by key.
    fn get_secret(&self, key: &str) -> Result<String, String>;

    /// Make an HTTP request with binary body. Returns (status_code, response_bytes).
    ///
    /// Used by streaming host functions where the request body and response are
    /// raw bytes (not UTF-8 strings). The host reads/writes bytes from/to
    /// StreamRegistry; WASM never touches raw binary data.
    async fn http_call_binary(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Result<(u16, Vec<u8>), String>;

    /// Make a Connect protocol server-streaming RPC call.
    ///
    /// Sends an HTTP POST with JSON body to the given URL using the Connect
    /// protocol (HTTP/1.1, `Connect-Protocol-Version: 1`). Reads the full
    /// response, parses Connect binary frames (5-byte prefix per message:
    /// 1 flag byte + 4 big-endian length bytes), and returns each data-frame
    /// payload as a JSON string.
    ///
    /// Returns a vec of decoded JSON message payloads.
    async fn connect_call(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &str,
    ) -> Result<Vec<String>, String> {
        let _ = (url, headers, body);
        Err("connect_call not supported by this host".to_string())
    }

    /// Log a message at the given level.
    fn log(&self, level: &str, message: &str);

    /// Evaluate a single transition against an IOA spec.
    ///
    /// Generic platform capability: any WASM module can validate transitions.
    /// The host builds a TransitionTable from the IOA source and evaluates
    /// the given action from the given state with the given parameters.
    ///
    /// Returns a JSON result: `{ "success": bool, "new_state": str, "error": str|null, "guard_result": str|null }`
    ///
    /// Default: not supported (overridden in temper-server where temper-jit is available).
    fn evaluate_spec(
        &self,
        _ioa_source: &str,
        _current_state: &str,
        _action: &str,
        _params_json: &str,
    ) -> Result<String, String> {
        Err("evaluate_spec not supported by this host".to_string())
    }

    /// Emit a replayable progress event from the guest module.
    fn emit_progress(&self, _event_json: &str) -> Result<(), String> {
        Ok(())
    }
}

/// Callback for evaluating IOA spec transitions.
///
/// Injected by `temper-server` where `temper-jit` is available.
/// Keeps the dependency boundary clean: `temper-wasm` never depends on `temper-jit`.
pub type SpecEvaluatorFn =
    Arc<dyn Fn(&str, &str, &str, &str) -> Result<String, String> + Send + Sync>;

/// Callback for replayable progress events emitted by guest WASM modules.
pub type ProgressEmitterFn = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

/// Production host: real HTTP calls via reqwest, real secrets.
pub struct ProductionWasmHost {
    /// HTTP client for making real requests.
    client: reqwest::Client,
    /// Secrets from env vars or a secret store.
    secrets: BTreeMap<String, String>,
    /// Optional spec evaluator (provided by temper-server at construction).
    spec_evaluator: Option<SpecEvaluatorFn>,
    /// Optional progress emitter (provided by temper-server at construction).
    progress_emitter: Option<ProgressEmitterFn>,
}

impl ProductionWasmHost {
    /// Create with pre-loaded secrets and default HTTP timeout.
    pub fn new(secrets: BTreeMap<String, String>) -> Self {
        Self::with_timeout(secrets, std::time::Duration::from_secs(30))
    }

    /// Create with pre-loaded secrets and a custom HTTP request timeout.
    pub fn with_timeout(secrets: BTreeMap<String, String>, timeout: std::time::Duration) -> Self {
        Self {
            client: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .timeout(timeout)
                .build()
                .unwrap_or_default(),
            secrets,
            spec_evaluator: None,
            progress_emitter: None,
        }
    }

    /// Create with a spec evaluator for `host_evaluate_spec` support.
    pub fn with_spec_evaluator(mut self, evaluator: SpecEvaluatorFn) -> Self {
        self.spec_evaluator = Some(evaluator);
        self
    }

    /// Create with a progress emitter for `host_emit_progress` support.
    pub fn with_progress_emitter(mut self, emitter: ProgressEmitterFn) -> Self {
        self.progress_emitter = Some(emitter);
        self
    }
}

#[async_trait]
impl WasmHost for ProductionWasmHost {
    async fn http_call(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: &str,
    ) -> Result<(u16, String), String> {
        let mut builder = match method.to_uppercase().as_str() {
            "GET" => self.client.get(url),
            "POST" => self.client.post(url),
            "PUT" => self.client.put(url),
            "DELETE" => self.client.delete(url),
            "PATCH" => self.client.patch(url),
            other => return Err(format!("unsupported HTTP method: {other}")),
        };

        for (k, v) in headers {
            builder = builder.header(k.as_str(), v.as_str());
        }

        if !body.is_empty() {
            builder = builder.body(body.to_string());
        }

        let resp = builder
            .send()
            .await
            .map_err(|e| format!("HTTP request failed: {e}"))?;
        let status = resp.status().as_u16();
        let resp_body = resp
            .text()
            .await
            .map_err(|e| format!("failed to read response body: {e}"))?;
        Ok((status, resp_body))
    }

    async fn http_call_binary(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Result<(u16, Vec<u8>), String> {
        let mut builder = match method.to_uppercase().as_str() {
            "GET" => self.client.get(url),
            "POST" => self.client.post(url),
            "PUT" => self.client.put(url),
            "DELETE" => self.client.delete(url),
            "PATCH" => self.client.patch(url),
            other => return Err(format!("unsupported HTTP method: {other}")),
        };

        for (k, v) in headers {
            builder = builder.header(k.as_str(), v.as_str());
        }

        if !body.is_empty() {
            builder = builder.body(body.to_vec());
        }

        let resp = builder
            .send()
            .await
            .map_err(|e| format!("HTTP binary request failed: {e}"))?;
        let status = resp.status().as_u16();
        let resp_bytes = resp
            .bytes()
            .await
            .map_err(|e| format!("failed to read binary response body: {e}"))?;
        Ok((status, resp_bytes.to_vec()))
    }

    async fn connect_call(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &str,
    ) -> Result<Vec<String>, String> {
        let mut builder = self.client.post(url);

        // Set Connect protocol headers.
        // Use application/connect+json for envd-compatible services (E2B, etc.)
        builder = builder
            .header("content-type", "application/connect+json")
            .header("connect-protocol-version", "1");

        for (k, v) in headers {
            builder = builder.header(k.as_str(), v.as_str());
        }

        if !body.is_empty() {
            builder = builder.body(encode_connect_json_frame(body));
        }

        let resp = builder
            .send()
            .await
            .map_err(|e| format!("Connect call failed: {e}"))?;

        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            let err_body = resp.text().await.unwrap_or_default();
            return Err(format!("Connect call failed (HTTP {status}): {err_body}"));
        }

        let resp_bytes = resp
            .bytes()
            .await
            .map_err(|e| format!("failed to read Connect response body: {e}"))?;

        parse_connect_frames(&resp_bytes)
    }

    fn get_secret(&self, key: &str) -> Result<String, String> {
        self.secrets
            .get(key)
            .cloned()
            .ok_or_else(|| format!("secret not found: {key}"))
    }

    fn log(&self, level: &str, message: &str) {
        match level {
            "error" => tracing::error!(target: "wasm_guest", "{}", message),
            "warn" => tracing::warn!(target: "wasm_guest", "{}", message),
            "info" => tracing::info!(target: "wasm_guest", "{}", message),
            _ => tracing::debug!(target: "wasm_guest", "{}", message),
        }
    }

    fn evaluate_spec(
        &self,
        ioa_source: &str,
        current_state: &str,
        action: &str,
        params_json: &str,
    ) -> Result<String, String> {
        match &self.spec_evaluator {
            Some(evaluator) => evaluator(ioa_source, current_state, action, params_json),
            None => Err("evaluate_spec not supported by this host".to_string()),
        }
    }

    fn emit_progress(&self, event_json: &str) -> Result<(), String> {
        match &self.progress_emitter {
            Some(emitter) => emitter(event_json),
            None => Ok(()),
        }
    }
}

/// Parse Connect protocol binary frames from a response body.
///
/// Each frame has a 5-byte prefix: 1 flag byte + 4 big-endian length bytes.
/// Flag 0x00 = data frame, flag 0x02 = trailer frame (end-of-stream).
/// Returns the payload of all data frames as strings.
pub fn parse_connect_frames(data: &[u8]) -> Result<Vec<String>, String> {
    let mut frames = Vec::new();
    let mut offset = 0;

    while offset < data.len() {
        if offset + 5 > data.len() {
            return Err(format!(
                "incomplete Connect frame header at offset {offset} (need 5 bytes, have {})",
                data.len() - offset
            ));
        }

        let flags = data[offset];
        let length = u32::from_be_bytes([
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
            data[offset + 4],
        ]) as usize;
        offset += 5;

        if offset + length > data.len() {
            return Err(format!(
                "incomplete Connect frame payload at offset {}: expected {length} bytes, have {}",
                offset - 5,
                data.len() - offset
            ));
        }

        let payload = &data[offset..offset + length];
        offset += length;

        // flags 0x00 = data frame, 0x02 = trailer/end-of-stream
        if flags & 0x02 == 0 {
            let payload_str = String::from_utf8(payload.to_vec())
                .map_err(|e| format!("Connect frame payload is not valid UTF-8: {e}"))?;
            frames.push(payload_str);
        }
        // Trailer frames (0x02) are skipped — they contain metadata, not data
    }

    Ok(frames)
}

/// Encode a JSON payload as a Connect protocol envelope.
///
/// Connect JSON still uses the 5-byte envelope framing: 1 flag byte followed by
/// a 4-byte big-endian payload length.
pub fn encode_connect_json_frame(body: &str) -> Vec<u8> {
    let payload = body.as_bytes();
    let mut framed = Vec::with_capacity(5 + payload.len());
    framed.push(0x00);
    framed.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    framed.extend_from_slice(payload);
    framed
}

/// Simulation host: canned responses, captured logs.
///
/// Uses `BTreeMap` for deterministic iteration (DST compliance).
pub struct SimWasmHost {
    /// Canned HTTP responses: URL pattern -> (status, body).
    responses: BTreeMap<String, (u16, String)>,
    /// Canned binary HTTP responses: URL pattern -> (status, bytes).
    binary_responses: BTreeMap<String, (u16, Vec<u8>)>,
    /// Canned Connect responses: URL pattern -> vec of frame payloads.
    connect_responses: BTreeMap<String, Vec<String>>,
    /// Canned secrets.
    secrets: BTreeMap<String, String>,
    /// Canned evaluate_spec responses: (ioa_source_hash, action) -> result JSON.
    spec_eval_responses: BTreeMap<(String, String), String>,
    /// Default response for URLs not in the map.
    default_response: (u16, String),
    /// Default binary response for URLs not in the binary map.
    default_binary_response: (u16, Vec<u8>),
}

impl SimWasmHost {
    /// Create a simulation host with default 200 OK responses.
    pub fn new() -> Self {
        Self {
            responses: BTreeMap::new(),
            binary_responses: BTreeMap::new(),
            connect_responses: BTreeMap::new(),
            secrets: BTreeMap::new(),
            spec_eval_responses: BTreeMap::new(),
            default_response: (200, r#"{"ok": true}"#.to_string()),
            default_binary_response: (200, Vec::new()),
        }
    }

    /// Add a canned HTTP response for a URL.
    pub fn with_response(mut self, url: &str, status: u16, body: &str) -> Self {
        self.responses
            .insert(url.to_string(), (status, body.to_string()));
        self
    }

    /// Add a canned binary HTTP response for a URL.
    pub fn with_binary_response(mut self, url: &str, status: u16, bytes: Vec<u8>) -> Self {
        self.binary_responses
            .insert(url.to_string(), (status, bytes));
        self
    }

    /// Add a canned Connect response for a URL.
    pub fn with_connect_response(mut self, url: &str, frames: Vec<String>) -> Self {
        self.connect_responses.insert(url.to_string(), frames);
        self
    }

    /// Add a canned secret.
    pub fn with_secret(mut self, key: &str, value: &str) -> Self {
        self.secrets.insert(key.to_string(), value.to_string());
        self
    }

    /// Set the default response for unmatched URLs.
    pub fn with_default_response(mut self, status: u16, body: &str) -> Self {
        self.default_response = (status, body.to_string());
        self
    }

    /// Set the default binary response for unmatched URLs.
    pub fn with_default_binary_response(mut self, status: u16, bytes: Vec<u8>) -> Self {
        self.default_binary_response = (status, bytes);
        self
    }

    /// Add a canned evaluate_spec response for a given action.
    pub fn with_spec_eval_response(
        mut self,
        ioa_hash: &str,
        action: &str,
        result_json: &str,
    ) -> Self {
        self.spec_eval_responses.insert(
            (ioa_hash.to_string(), action.to_string()),
            result_json.to_string(),
        );
        self
    }
}

impl Default for SimWasmHost {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl WasmHost for SimWasmHost {
    async fn http_call(
        &self,
        _method: &str,
        url: &str,
        _headers: &[(String, String)],
        _body: &str,
    ) -> Result<(u16, String), String> {
        let (status, body) = self
            .responses
            .get(url)
            .cloned()
            .unwrap_or_else(|| self.default_response.clone());
        Ok((status, body))
    }

    async fn http_call_binary(
        &self,
        _method: &str,
        url: &str,
        _headers: &[(String, String)],
        _body: &[u8],
    ) -> Result<(u16, Vec<u8>), String> {
        let (status, bytes) = self
            .binary_responses
            .get(url)
            .cloned()
            .unwrap_or_else(|| self.default_binary_response.clone());
        Ok((status, bytes))
    }

    async fn connect_call(
        &self,
        url: &str,
        _headers: &[(String, String)],
        _body: &str,
    ) -> Result<Vec<String>, String> {
        Ok(self.connect_responses.get(url).cloned().unwrap_or_default())
    }

    fn get_secret(&self, key: &str) -> Result<String, String> {
        self.secrets
            .get(key)
            .cloned()
            .ok_or_else(|| format!("sim secret not found: {key}"))
    }

    fn log(&self, level: &str, message: &str) {
        tracing::debug!(target: "wasm_guest_sim", level = level, "{}", message);
    }

    fn evaluate_spec(
        &self,
        ioa_source: &str,
        _current_state: &str,
        action: &str,
        _params_json: &str,
    ) -> Result<String, String> {
        // Use a simple hash of the IOA source for lookup
        let hash = format!("{:x}", ioa_source.len());
        self.spec_eval_responses
            .get(&(hash, action.to_string()))
            .cloned()
            .ok_or_else(|| format!("sim: no canned response for action '{action}'"))
    }

    fn emit_progress(&self, _event_json: &str) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a Connect frame: [flags(1)][length(4 big-endian)][payload].
    fn make_frame(flags: u8, payload: &[u8]) -> Vec<u8> {
        let mut frame = Vec::with_capacity(5 + payload.len());
        frame.push(flags);
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(payload);
        frame
    }

    #[test]
    fn parse_single_data_frame() {
        let payload = b"{\"stdout\":\"hello\"}";
        let data = make_frame(0x00, payload);
        let frames = parse_connect_frames(&data).unwrap();
        assert_eq!(frames, vec!["{\"stdout\":\"hello\"}"]);
    }

    #[test]
    fn parse_multiple_frames() {
        let mut data = make_frame(0x00, b"{\"stdout\":\"line1\"}");
        data.extend(make_frame(0x00, b"{\"stdout\":\"line2\"}"));
        data.extend(make_frame(0x02, b"trailer")); // trailer frame, should be skipped
        let frames = parse_connect_frames(&data).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0], "{\"stdout\":\"line1\"}");
        assert_eq!(frames[1], "{\"stdout\":\"line2\"}");
    }

    #[test]
    fn parse_empty_input() {
        let frames = parse_connect_frames(&[]).unwrap();
        assert!(frames.is_empty());
    }

    #[test]
    fn encode_connect_json_frame_wraps_payload() {
        let payload = "{\"hello\":\"world\"}";
        let framed = encode_connect_json_frame(payload);
        assert_eq!(framed[0], 0x00);
        assert_eq!(
            u32::from_be_bytes([framed[1], framed[2], framed[3], framed[4]]) as usize,
            payload.len()
        );
        assert_eq!(&framed[5..], payload.as_bytes());
    }

    #[test]
    fn parse_trailer_only() {
        let data = make_frame(0x02, b"{}");
        let frames = parse_connect_frames(&data).unwrap();
        assert!(frames.is_empty());
    }

    #[test]
    fn parse_incomplete_header_errors() {
        let result = parse_connect_frames(&[0x00, 0x00]);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("incomplete Connect frame header")
        );
    }

    #[test]
    fn parse_incomplete_payload_errors() {
        // Header says 100 bytes but only 3 available
        let mut data = vec![0x00];
        data.extend_from_slice(&100u32.to_be_bytes());
        data.extend_from_slice(b"abc");
        let result = parse_connect_frames(&data);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("incomplete Connect frame payload")
        );
    }
}
