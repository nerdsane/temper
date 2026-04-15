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

    /// Build the [`TemperClient`].
    pub fn build(self) -> Result<TemperClient> {
        anyhow::ensure!(!self.base_url.is_empty(), "base_url is required");
        anyhow::ensure!(!self.tenant.is_empty(), "tenant is required");

        Ok(TemperClient {
            base_url: self.base_url,
            tenant: self.tenant,
            principal: self.principal,
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
    http: reqwest::Client,
}

impl TemperClient {
    /// Create a new [`ClientBuilder`].
    pub fn builder() -> ClientBuilder {
        ClientBuilder {
            base_url: String::new(),
            tenant: "default".to_string(),
            principal: None,
        }
    }

    /// Convenience constructor for simple cases (equivalent to builder with defaults).
    pub fn new(base_url: &str, tenant: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            tenant: tenant.to_string(),
            principal: None,
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

    // ── Entity CRUD ──────────────────────────────────────────────────

    /// List all entities of the given type.
    pub async fn list(&self, entity_type: &str) -> Result<Vec<Value>> {
        let url = format!("{}/tdata/{entity_type}", self.base_url);
        let resp = self
            .http
            .get(&url)
            .header("x-tenant-id", &self.tenant)
            .send()
            .await
            .with_context(|| format!("Failed to list {entity_type}"))?;

        self.check_status(&resp, "list", entity_type)?;
        let body: Value = resp.json().await.context("Failed to parse list response")?;
        Ok(body
            .get("value")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default())
    }

    /// List entities with an OData `$filter` expression.
    pub async fn list_filtered(&self, entity_type: &str, filter: &str) -> Result<Vec<Value>> {
        let url = format!("{}/tdata/{entity_type}", self.base_url);
        let resp = self
            .http
            .get(&url)
            .header("x-tenant-id", &self.tenant)
            .query(&[("$filter", filter)])
            .send()
            .await
            .with_context(|| format!("Failed to list_filtered {entity_type}"))?;

        self.check_status(&resp, "list_filtered", entity_type)?;
        let body: Value = resp
            .json()
            .await
            .context("Failed to parse list_filtered response")?;
        Ok(body
            .get("value")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default())
    }

    /// Get a single entity by type and ID.
    pub async fn get(&self, entity_type: &str, id: &str) -> Result<Value> {
        let url = format!("{}/tdata/{entity_type}('{id}')", self.base_url);
        let resp = self
            .http
            .get(&url)
            .header("x-tenant-id", &self.tenant)
            .send()
            .await
            .with_context(|| format!("Failed to get {entity_type}('{id}')"))?;

        self.check_status(&resp, "get", entity_type)?;
        resp.json().await.context("Failed to parse get response")
    }

    /// Create a new entity.
    pub async fn create(&self, entity_type: &str, fields: Value) -> Result<Value> {
        let url = format!("{}/tdata/{entity_type}", self.base_url);
        let mut req = self
            .http
            .post(&url)
            .header("x-tenant-id", &self.tenant)
            .json(&fields);

        if let Some(principal) = &self.principal {
            req = req
                .header("x-temper-principal-id", principal)
                .header("x-temper-principal-kind", "Agent");
        }

        let resp = req
            .send()
            .await
            .with_context(|| format!("Failed to create {entity_type}"))?;

        self.check_status(&resp, "create", entity_type)?;
        resp.json().await.context("Failed to parse create response")
    }

    /// Patch (update) an existing entity's fields.
    pub async fn patch(&self, entity_type: &str, id: &str, fields: Value) -> Result<Value> {
        let url = format!("{}/tdata/{entity_type}('{id}')", self.base_url);
        let resp = self
            .http
            .patch(&url)
            .header("x-tenant-id", &self.tenant)
            .json(&fields)
            .send()
            .await
            .with_context(|| format!("Failed to patch {entity_type}('{id}')"))?;

        self.check_status(&resp, "patch", entity_type)?;
        resp.json().await.context("Failed to parse patch response")
    }

    /// Invoke an OData action on an entity.
    pub async fn action(
        &self,
        entity_type: &str,
        id: &str,
        action: &str,
        params: Value,
    ) -> Result<Value> {
        let url = format!(
            "{}/tdata/{entity_type}('{id}')/Temper.{action}",
            self.base_url
        );
        let resp = self
            .http
            .post(&url)
            .header("x-tenant-id", &self.tenant)
            .json(&params)
            .send()
            .await
            .with_context(|| format!("Failed to invoke {entity_type}.{action}"))?;

        self.check_status(&resp, "action", &format!("{entity_type}.{action}"))?;
        resp.json()
            .await
            .with_context(|| format!("Failed to parse {entity_type}.{action} response"))
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
            .http
            .post(&url)
            .header("x-tenant-id", &self.tenant)
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
        self.http
            .post(&url)
            .header("x-tenant-id", &self.tenant)
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
            .http
            .get(&url)
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
            .http
            .post(&url)
            .header("x-tenant-id", &self.tenant)
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
            .http
            .get(&url)
            .header("x-tenant-id", &self.tenant)
            .header("Accept", "text/event-stream")
            .send()
            .await
            .context("Failed to connect to SSE endpoint")?;

        Ok(parse_sse_stream(resp.bytes_stream()))
    }

    // ── Helpers ──────────────────────────────────────────────────────

    /// Check HTTP response status code and return a descriptive error on failure.
    ///
    /// Note: consumes the response body on error, so this must be called before
    /// reading the response body. We take `&Response` and only read body on error.
    fn check_status(&self, resp: &reqwest::Response, operation: &str, context: &str) -> Result<()> {
        if !resp.status().is_success() {
            anyhow::bail!("{operation} {context} failed with status {}", resp.status());
        }
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builder_defaults() {
        let client = TemperClient::builder()
            .base_url("http://localhost:4200")
            .build()
            .unwrap();
        assert_eq!(client.base_url(), "http://localhost:4200");
        assert_eq!(client.tenant(), "default");
        assert!(client.principal().is_none());
    }

    #[test]
    fn test_builder_all_fields() {
        let client = TemperClient::builder()
            .base_url("http://localhost:4200/")
            .tenant("acme")
            .principal("agent-1")
            .build()
            .unwrap();
        assert_eq!(client.base_url(), "http://localhost:4200");
        assert_eq!(client.tenant(), "acme");
        assert_eq!(client.principal(), Some("agent-1"));
    }

    #[test]
    fn test_builder_requires_base_url() {
        let result = TemperClient::builder().build();
        assert!(result.is_err());
    }

    #[test]
    fn test_new_convenience() {
        let client = TemperClient::new("http://localhost:4200", "default");
        assert_eq!(client.base_url(), "http://localhost:4200");
        assert_eq!(client.tenant(), "default");
    }

    #[test]
    fn test_entity_url() {
        let client = TemperClient::new("http://localhost:4200", "default");
        assert_eq!(
            client.entity_url("Tasks"),
            "http://localhost:4200/tdata/Tasks"
        );
    }

    #[test]
    fn test_entity_instance_url() {
        let client = TemperClient::new("http://localhost:4200", "default");
        assert_eq!(
            client.entity_instance_url("Tasks", "t-1"),
            "http://localhost:4200/tdata/Tasks('t-1')"
        );
    }

    #[test]
    fn test_action_url() {
        let client = TemperClient::new("http://localhost:4200", "default");
        assert_eq!(
            client.action_url("Tasks", "t-1", "Start"),
            "http://localhost:4200/tdata/Tasks('t-1')/Temper.Start"
        );
    }

    #[test]
    fn test_trailing_slash_stripped() {
        let client = TemperClient::new("http://localhost:4200/", "default");
        assert_eq!(client.base_url(), "http://localhost:4200");
        assert_eq!(
            client.entity_url("Agents"),
            "http://localhost:4200/tdata/Agents"
        );
    }
}
