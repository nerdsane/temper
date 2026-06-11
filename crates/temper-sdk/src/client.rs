//! HTTP client for Temper server entity operations and governance.

use anyhow::{Context, Result};
use futures_util::Stream;
use serde_json::Value;

use crate::sse::parse_sse_stream;
use crate::types::{AuditEntry, AuthzResponse, EntityEvent};

/// Builder for constructing a [`TemperClient`].
pub struct ClientBuilder {
    base_url: String,
    tenant: String,
    principal: Option<String>,
    principal_kind: Option<String>,
    api_key: Option<String>,
}

impl ClientBuilder {
    /// Set the Temper server base URL (e.g., `http://127.0.0.1:4200`).
    pub fn base_url(mut self, url: &str) -> Self {
        self.base_url = url.trim_end_matches('/').to_string();
        self
    }

    /// Set the tenant ID for multi-tenant scoping.
    pub fn tenant(mut self, tenant: &str) -> Self {
        self.tenant = tenant.to_string();
        self
    }

    /// Set the principal ID for Cedar authorization headers.
    pub fn principal(mut self, principal: &str) -> Self {
        self.principal = Some(principal.to_string());
        self
    }

    /// Set the principal kind header (e.g., `"admin"`).
    ///
    /// Defaults to `"Agent"` when a principal ID is set, and is omitted
    /// entirely when neither a principal ID nor a kind is configured.
    pub fn principal_kind(mut self, kind: &str) -> Self {
        self.principal_kind = Some(kind.to_string());
        self
    }

    /// Set an API key sent as a `Bearer` token on every request.
    pub fn api_key(mut self, key: &str) -> Self {
        self.api_key = Some(key.to_string());
        self
    }

    /// Build the [`TemperClient`].
    pub fn build(self) -> Result<TemperClient> {
        anyhow::ensure!(!self.base_url.is_empty(), "base_url is required");
        anyhow::ensure!(!self.tenant.is_empty(), "tenant is required");

        Ok(TemperClient {
            base_url: self.base_url,
            tenant: self.tenant,
            principal: self.principal,
            principal_kind: self.principal_kind,
            api_key: self.api_key,
            http: reqwest::Client::new(),
        })
    }
}

/// Thin HTTP client for Temper server entity operations.
///
/// Mirrors the dispatch surface of `temper-mcp`: entity CRUD, governance,
/// spec management, and SSE event streaming.
pub struct TemperClient {
    base_url: String,
    tenant: String,
    principal: Option<String>,
    principal_kind: Option<String>,
    api_key: Option<String>,
    http: reqwest::Client,
}

impl TemperClient {
    /// Create a new [`ClientBuilder`].
    pub fn builder() -> ClientBuilder {
        ClientBuilder {
            base_url: String::new(),
            tenant: "default".to_string(),
            principal: None,
            principal_kind: None,
            api_key: None,
        }
    }

    /// Convenience constructor for simple cases (equivalent to builder with defaults).
    pub fn new(base_url: &str, tenant: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            tenant: tenant.to_string(),
            principal: None,
            principal_kind: None,
            api_key: None,
            http: reqwest::Client::new(),
        }
    }

    /// Returns the configured base URL.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Returns the configured tenant ID.
    pub fn tenant(&self) -> &str {
        &self.tenant
    }

    /// Returns the configured principal ID, if any.
    pub fn principal(&self) -> Option<&str> {
        self.principal.as_deref()
    }

    /// Returns the configured principal kind override, if any.
    pub fn principal_kind(&self) -> Option<&str> {
        self.principal_kind.as_deref()
    }

    /// Returns the configured API key, if any.
    pub fn api_key(&self) -> Option<&str> {
        self.api_key.as_deref()
    }

    // ── Entity CRUD ──────────────────────────────────────────────────

    /// List all entities of the given type.
    pub async fn list(&self, entity_type: &str) -> Result<Vec<Value>> {
        let url = self.entity_url(entity_type);
        let resp = self
            .request(reqwest::Method::GET, &url)
            .send()
            .await
            .with_context(|| format!("Failed to list {entity_type}"))?;

        let resp = ensure_success(resp, "list", entity_type).await?;
        let body: Value = resp.json().await.context("Failed to parse list response")?;
        Ok(values_array(&body))
    }

    /// List entities with an OData `$filter` expression.
    pub async fn list_filtered(&self, entity_type: &str, filter: &str) -> Result<Vec<Value>> {
        let url = self.entity_url(entity_type);
        let resp = self
            .request(reqwest::Method::GET, &url)
            .query(&[("$filter", filter)])
            .send()
            .await
            .with_context(|| format!("Failed to list_filtered {entity_type}"))?;

        let resp = ensure_success(resp, "list_filtered", entity_type).await?;
        let body: Value = resp
            .json()
            .await
            .context("Failed to parse list_filtered response")?;
        Ok(values_array(&body))
    }

    /// Get a single entity by type and ID.
    pub async fn get(&self, entity_type: &str, id: &str) -> Result<Value> {
        let url = self.entity_instance_url(entity_type, id);
        let resp = self
            .request(reqwest::Method::GET, &url)
            .send()
            .await
            .with_context(|| format!("Failed to get {entity_type}('{id}')"))?;

        let resp = ensure_success(resp, "get", entity_type).await?;
        resp.json().await.context("Failed to parse get response")
    }

    /// Create a new entity.
    pub async fn create(&self, entity_type: &str, fields: Value) -> Result<Value> {
        let url = self.entity_url(entity_type);
        let resp = self
            .request(reqwest::Method::POST, &url)
            .json(&fields)
            .send()
            .await
            .with_context(|| format!("Failed to create {entity_type}"))?;

        let resp = ensure_success(resp, "create", entity_type).await?;
        resp.json().await.context("Failed to parse create response")
    }

    /// Patch (update) an existing entity's fields.
    pub async fn patch(&self, entity_type: &str, id: &str, fields: Value) -> Result<Value> {
        let url = self.entity_instance_url(entity_type, id);
        let resp = self
            .request(reqwest::Method::PATCH, &url)
            .json(&fields)
            .send()
            .await
            .with_context(|| format!("Failed to patch {entity_type}('{id}')"))?;

        let resp = ensure_success(resp, "patch", entity_type).await?;
        resp.json().await.context("Failed to parse patch response")
    }

    /// Invoke an OData action on an entity.
    ///
    /// The `Temper.` namespace prefix is added automatically, so pass
    /// `"Start"` for `Temper.Start` or `"Claw.Channel.Connect"` for
    /// `Temper.Claw.Channel.Connect`.
    pub async fn action(
        &self,
        entity_type: &str,
        id: &str,
        action: &str,
        params: Value,
    ) -> Result<Value> {
        let url = self.action_url(entity_type, id, action);
        let resp = self
            .request(reqwest::Method::POST, &url)
            .json(&params)
            .send()
            .await
            .with_context(|| format!("Failed to invoke {entity_type}.{action}"))?;

        let resp = ensure_success(resp, "action", &format!("{entity_type}.{action}")).await?;
        resp.json()
            .await
            .with_context(|| format!("Failed to parse {entity_type}.{action} response"))
    }

    /// POST an arbitrary JSON body to a fully-qualified URL with the
    /// client's standard tenant/auth/principal headers.
    ///
    /// Escape hatch for server endpoints not covered by a dedicated method.
    pub async fn raw_post(&self, url: &str, body: Value) -> Result<Value> {
        let resp = self
            .request(reqwest::Method::POST, url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("Failed to POST {url}"))?;

        let resp = ensure_success(resp, "POST", url).await?;
        resp.json().await.context("Failed to parse POST response")
    }

    /// Install an OS app for the client's tenant (idempotent server-side).
    pub async fn install_app(&self, app: &str) -> Result<Value> {
        let url = format!("{}/tdata/_install_app", self.base_url);
        self.raw_post(&url, serde_json::json!({ "app": app })).await
    }

    // ── Governance ───────────────────────────────────────────────────

    /// Check Cedar authorization for an action.
    pub async fn authorize(
        &self,
        agent_id: &str,
        action: &str,
        resource_type: &str,
        resource_id: &str,
    ) -> Result<AuthzResponse> {
        let url = format!("{}/api/authorize", self.base_url);
        let body = serde_json::json!({
            "agent_id": agent_id,
            "action": action,
            "resource_type": resource_type,
            "resource_id": resource_id,
        });

        let resp = self
            .request_base(reqwest::Method::POST, &url)
            .header("x-temper-principal-id", agent_id)
            .header("x-temper-principal-kind", "Agent")
            .json(&body)
            .send()
            .await
            .context("Failed to call /api/authorize")?;

        let resp_json: Value = resp
            .json()
            .await
            .context("Failed to parse authorize response")?;

        Ok(AuthzResponse {
            allowed: resp_json
                .get("allowed")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            decision_id: resp_json
                .get("decision_id")
                .and_then(|v| v.as_str())
                .map(String::from),
            reason: resp_json
                .get("reason")
                .and_then(|v| v.as_str())
                .map(String::from),
        })
    }

    /// Submit an audit trail entry.
    pub async fn audit(&self, entry: AuditEntry) -> Result<()> {
        let url = format!("{}/api/audit", self.base_url);
        self.request_base(reqwest::Method::POST, &url)
            .json(&entry)
            .send()
            .await
            .context("Failed to submit audit entry")?;
        Ok(())
    }

    /// Get governance decisions, optionally filtered by status.
    pub async fn get_decisions(&self, status: Option<&str>) -> Result<Vec<Value>> {
        let url = match status {
            Some(s) => format!(
                "{}/api/tenants/{}/decisions?status={s}",
                self.base_url, self.tenant
            ),
            None => format!("{}/api/tenants/{}/decisions", self.base_url, self.tenant),
        };

        let resp = self
            .request_base(reqwest::Method::GET, &url)
            .header("Accept", "application/json")
            .send()
            .await
            .context("Failed to fetch decisions")?;

        let body: Value = resp
            .json()
            .await
            .context("Failed to parse decisions response")?;
        Ok(body
            .get("decisions")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default())
    }

    // ── Specs ────────────────────────────────────────────────────────

    /// Submit specs to the Temper server.
    pub async fn submit_specs(&self, specs: Value) -> Result<Value> {
        let url = format!("{}/api/specs", self.base_url);
        let resp = self
            .request_base(reqwest::Method::POST, &url)
            .json(&specs)
            .send()
            .await
            .context("Failed to submit specs")?;

        resp.json().await.context("Failed to parse specs response")
    }

    // ── SSE ──────────────────────────────────────────────────────────

    /// Open an SSE connection and return a stream of entity events.
    pub async fn events_stream(&self) -> Result<impl Stream<Item = Result<EntityEvent>>> {
        let url = format!("{}/api/events", self.base_url);
        let resp = self
            .request_base(reqwest::Method::GET, &url)
            .header("Accept", "text/event-stream")
            .send()
            .await
            .context("Failed to connect to SSE endpoint")?;

        Ok(parse_sse_stream(resp.bytes_stream()))
    }

    // ── Helpers ──────────────────────────────────────────────────────

    /// Build a request with tenant and bearer-auth headers (no principal headers).
    fn request_base(&self, method: reqwest::Method, url: &str) -> reqwest::RequestBuilder {
        let mut req = self
            .http
            .request(method, url)
            .header("x-tenant-id", &self.tenant);
        if let Some(key) = &self.api_key {
            req = req.header("authorization", format!("Bearer {key}"));
        }
        req
    }

    /// Build a request with tenant, auth, and principal headers.
    fn request(&self, method: reqwest::Method, url: &str) -> reqwest::RequestBuilder {
        let mut req = self.request_base(method, url);
        if let Some(principal) = &self.principal {
            req = req.header("x-temper-principal-id", principal);
        }
        match (&self.principal_kind, &self.principal) {
            (Some(kind), _) => req = req.header("x-temper-principal-kind", kind),
            (None, Some(_)) => req = req.header("x-temper-principal-kind", "Agent"),
            (None, None) => {}
        }
        req
    }

    /// Build the entity URL for a given entity type.
    pub fn entity_url(&self, entity_type: &str) -> String {
        format!("{}/tdata/{entity_type}", self.base_url)
    }

    /// Build the entity instance URL for a given entity type and ID.
    pub fn entity_instance_url(&self, entity_type: &str, id: &str) -> String {
        format!("{}/tdata/{entity_type}('{id}')", self.base_url)
    }

    /// Build the action URL for a given entity type, ID, and action name.
    pub fn action_url(&self, entity_type: &str, id: &str, action: &str) -> String {
        format!(
            "{}/tdata/{entity_type}('{id}')/Temper.{action}",
            self.base_url
        )
    }
}

/// Check the HTTP status, returning a descriptive error (including the
/// response body) on failure.
async fn ensure_success(
    resp: reqwest::Response,
    operation: &str,
    context: &str,
) -> Result<reqwest::Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.text().await.unwrap_or_default();
    anyhow::bail!("{operation} {context} failed with status {status}: {body}")
}

/// Extract the OData `value` array from a collection response body.
fn values_array(body: &Value) -> Vec<Value> {
    body.get("value")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}

#[cfg(test)]
#[path = "client_test.rs"]
mod tests;
