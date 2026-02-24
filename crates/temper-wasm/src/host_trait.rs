//! DST-compliant host function trait and implementations.
//!
//! Production uses real HTTP + secret store. Simulation uses canned
//! responses for deterministic testing.

use std::collections::BTreeMap;

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

    /// Log a message at the given level.
    fn log(&self, level: &str, message: &str);
}

/// Production host: real HTTP calls via reqwest, real secrets.
pub struct ProductionWasmHost {
    /// HTTP client for making real requests.
    client: reqwest::Client,
    /// Secrets from env vars or a secret store.
    secrets: BTreeMap<String, String>,
}

impl ProductionWasmHost {
    /// Create with pre-loaded secrets.
    pub fn new(secrets: BTreeMap<String, String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            secrets,
        }
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
}

/// Simulation host: canned responses, captured logs.
///
/// Uses `BTreeMap` for deterministic iteration (DST compliance).
pub struct SimWasmHost {
    /// Canned HTTP responses: URL pattern -> (status, body).
    responses: BTreeMap<String, (u16, String)>,
    /// Canned secrets.
    secrets: BTreeMap<String, String>,
    /// Default response for URLs not in the map.
    default_response: (u16, String),
}

impl SimWasmHost {
    /// Create a simulation host with default 200 OK responses.
    pub fn new() -> Self {
        Self {
            responses: BTreeMap::new(),
            secrets: BTreeMap::new(),
            default_response: (200, r#"{"ok": true}"#.to_string()),
        }
    }

    /// Add a canned HTTP response for a URL.
    pub fn with_response(mut self, url: &str, status: u16, body: &str) -> Self {
        self.responses
            .insert(url.to_string(), (status, body.to_string()));
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

    fn get_secret(&self, key: &str) -> Result<String, String> {
        self.secrets
            .get(key)
            .cloned()
            .ok_or_else(|| format!("sim secret not found: {key}"))
    }

    fn log(&self, level: &str, message: &str) {
        tracing::debug!(target: "wasm_guest_sim", level = level, "{}", message);
    }
}
