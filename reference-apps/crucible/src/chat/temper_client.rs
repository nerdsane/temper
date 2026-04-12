//! Thin OData-over-HTTP client for the subset of Crucible endpoints
//! the Phase 4 chat loop needs.
//!
//! This module is deliberately minimal: it knows about exactly the
//! entities (`Environment`, `ManagedAgent`, `Session`, `SessionEvent`)
//! and bound actions (`StartSession`, `IdleSession`, `ResumeSession`)
//! that the turn loop in [`crate::chat::responder`] and the fixture
//! helper in [`crate::chat::seed`] need to call. It does **not** try
//! to be a general-purpose Temper SDK.
//!
//! Every row struct mirrors the CSDL column names in
//! `reference-apps/crucible/specs/model.csdl.xml` exactly — the wire
//! format is PascalCase (with `id` lowercase for the primary key on
//! create, matching what the existing `SESSIONS_CURL_WALKTHROUGH.md`
//! and `SESSION_EVENTS_CURL_WALKTHROUGH.md` show). Nullable columns
//! map to `Option<T>` with `#[serde(skip_serializing_if = "Option::is_none")]`
//! so we do not send stray `null`s that would trip field invariants.

use anyhow::{Context, Result, anyhow};
use reqwest::{Client, Method, StatusCode};
use serde::{Deserialize, Serialize};

/// A Temper OData endpoint scoped to a single tenant.
///
/// `base_url` is something like `http://127.0.0.1:3000` (no trailing
/// slash). `tenant` is the `X-Tenant-Id` header every request must
/// carry — Crucible fixtures use `crucible`.
#[derive(Debug, Clone)]
pub struct TemperClient {
    base_url: String,
    tenant: String,
    http: Client,
}

impl TemperClient {
    pub fn new(base_url: impl Into<String>, tenant: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            tenant: tenant.into(),
            http: Client::new(),
        }
    }

    pub fn tenant(&self) -> &str {
        &self.tenant
    }

    // ------------------------------------------------------------------
    // Low-level HTTP helpers
    // ------------------------------------------------------------------

    fn url(&self, path: &str) -> String {
        if path.starts_with('/') {
            format!("{}{}", self.base_url, path)
        } else {
            format!("{}/{}", self.base_url, path)
        }
    }

    async fn send_json(
        &self,
        method: Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<(StatusCode, serde_json::Value)> {
        let url = self.url(path);
        let mut req = self
            .http
            .request(method.clone(), &url)
            .header("X-Tenant-Id", &self.tenant);
        if let Some(b) = &body {
            req = req.header("Content-Type", "application/json").json(b);
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("HTTP {method} {url} send failed"))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .with_context(|| format!("HTTP {method} {url} body read failed"))?;
        let value = if text.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_str(&text).with_context(|| {
                format!("HTTP {method} {url} → {status}: body was not JSON: {text}")
            })?
        };
        Ok((status, value))
    }

    async fn get(&self, path: &str) -> Result<(StatusCode, serde_json::Value)> {
        self.send_json(Method::GET, path, None).await
    }

    async fn post(
        &self,
        path: &str,
        body: serde_json::Value,
    ) -> Result<(StatusCode, serde_json::Value)> {
        self.send_json(Method::POST, path, Some(body)).await
    }

    fn require_2xx(
        method: &str,
        path: &str,
        status: StatusCode,
        body: &serde_json::Value,
    ) -> Result<()> {
        if !status.is_success() {
            return Err(anyhow!(
                "HTTP {method} {path} → {status}: {body}",
                method = method,
                path = path,
                status = status,
                body = body,
            ));
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Envelope decoding
    // ------------------------------------------------------------------

    fn decode_entity<T>(body: serde_json::Value) -> Result<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        // Single-entity GET/POST bodies look like:
        //   { "entity_type":"...", "entity_id":"...", "status":"...",
        //     "fields": { ...row fields... }, "@odata.id": "...", ... }
        // POST responses also include "events":[...] and PATCH sometimes
        // returns only {"status":"..."} — we always go through `fields`.
        let fields = body
            .get("fields")
            .cloned()
            .ok_or_else(|| anyhow!("response missing `fields` envelope: {body}"))?;
        serde_json::from_value(fields.clone())
            .with_context(|| format!("decoding entity row from: {fields}"))
    }

    fn decode_entity_list<T>(body: serde_json::Value) -> Result<Vec<T>>
    where
        T: for<'de> Deserialize<'de>,
    {
        // List bodies look like:
        //   { "@odata.context": "...", "value": [ {entity_envelope}, ... ] }
        let values = body
            .get("value")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow!("list response missing `value` array: {body}"))?;
        let mut out = Vec::with_capacity(values.len());
        for item in values {
            let fields = item
                .get("fields")
                .cloned()
                .ok_or_else(|| anyhow!("list entry missing `fields`: {item}"))?;
            let row: T = serde_json::from_value(fields.clone())
                .with_context(|| format!("decoding list row from: {fields}"))?;
            out.push(row);
        }
        Ok(out)
    }

    // ------------------------------------------------------------------
    // Environment
    // ------------------------------------------------------------------

    pub async fn create_environment(&self, row: &EnvironmentRow) -> Result<()> {
        let (status, body) = self
            .post(
                "/tdata/Environments",
                serde_json::to_value(row).context("serializing EnvironmentRow")?,
            )
            .await?;
        Self::require_2xx("POST", "/tdata/Environments", status, &body)
    }

    // ------------------------------------------------------------------
    // ManagedAgent
    // ------------------------------------------------------------------

    pub async fn create_managed_agent(&self, row: &ManagedAgentRow) -> Result<()> {
        let (status, body) = self
            .post(
                "/tdata/ManagedAgents",
                serde_json::to_value(row).context("serializing ManagedAgentRow")?,
            )
            .await?;
        Self::require_2xx("POST", "/tdata/ManagedAgents", status, &body)
    }

    pub async fn get_managed_agent(&self, id: &str) -> Result<ManagedAgentRow> {
        let path = format!("/tdata/ManagedAgents('{id}')");
        let (status, body) = self.get(&path).await?;
        Self::require_2xx("GET", &path, status, &body)?;
        Self::decode_entity(body)
    }

    // ------------------------------------------------------------------
    // Session
    // ------------------------------------------------------------------

    pub async fn create_session(&self, row: &SessionRow) -> Result<()> {
        let (status, body) = self
            .post(
                "/tdata/Sessions",
                serde_json::to_value(row).context("serializing SessionRow")?,
            )
            .await?;
        Self::require_2xx("POST", "/tdata/Sessions", status, &body)
    }

    pub async fn get_session(&self, id: &str) -> Result<SessionRow> {
        let path = format!("/tdata/Sessions('{id}')");
        let (status, body) = self.get(&path).await?;
        Self::require_2xx("GET", &path, status, &body)?;
        Self::decode_entity(body)
    }

    /// POST a bound lifecycle action on a Session. Returns the updated
    /// row so callers can inspect the new `Status`.
    pub async fn invoke_session_action(
        &self,
        session_id: &str,
        action: SessionAction,
    ) -> Result<()> {
        let path = format!(
            "/tdata/Sessions('{session_id}')/Temper.Crucible.{action_name}",
            action_name = action.as_str(),
        );
        let (status, body) = self.post(&path, serde_json::json!({})).await?;
        Self::require_2xx("POST", &path, status, &body)
    }

    // ------------------------------------------------------------------
    // SessionEvent
    // ------------------------------------------------------------------

    /// List events for a session, ordered by `Sequence asc`, capped at
    /// `top` rows. The turn loop uses `top = 500` which is more than
    /// enough headroom for an MVP chat (Anthropic will trim context
    /// long before we hit that).
    pub async fn list_session_events(
        &self,
        session_id: &str,
        top: u32,
    ) -> Result<Vec<SessionEventRow>> {
        // URL-encode the single quote in the OData filter literal by
        // using %27 — matching the existing test harness in
        // crucible_sessions_validation.rs.
        let path = format!(
            "/tdata/SessionEvents?$filter=SessionId%20eq%20%27{session_id}%27&$orderby=Sequence%20asc&$top={top}"
        );
        let (status, body) = self.get(&path).await?;
        Self::require_2xx("GET", &path, status, &body)?;
        Self::decode_entity_list(body)
    }

    /// List events with `Sequence > min_sequence`. Used by the polling
    /// loop to detect new events since the last check.
    pub async fn list_events_after(
        &self,
        session_id: &str,
        min_sequence: i64,
        top: u32,
    ) -> Result<Vec<SessionEventRow>> {
        let path = format!(
            "/tdata/SessionEvents?$filter=SessionId%20eq%20%27{session_id}%27%20and%20Sequence%20gt%20{min_sequence}&$orderby=Sequence%20asc&$top={top}"
        );
        let (status, body) = self.get(&path).await?;
        Self::require_2xx("GET", &path, status, &body)?;
        Self::decode_entity_list(body)
    }

    pub async fn create_session_event(&self, row: &SessionEventRow) -> Result<()> {
        let (status, body) = self
            .post(
                "/tdata/SessionEvents",
                serde_json::to_value(row).context("serializing SessionEventRow")?,
            )
            .await?;
        Self::require_2xx("POST", "/tdata/SessionEvents", status, &body)
    }

    // ------------------------------------------------------------------
    // AgentTool
    // ------------------------------------------------------------------

    pub async fn create_agent_tool(&self, row: &AgentToolRow) -> Result<()> {
        let (status, body) = self
            .post(
                "/tdata/AgentTools",
                serde_json::to_value(row).context("serializing AgentToolRow")?,
            )
            .await?;
        Self::require_2xx("POST", "/tdata/AgentTools", status, &body)
    }
}

// ======================================================================
// Bound actions
// ======================================================================

/// The subset of Session bound actions Phase 4 drives from the
/// responder. The omitted actions (`RescheduleSession`, `TerminateSession`,
/// `ArchiveSession`) are deliberately not exposed here — the MVP loop
/// never calls them.
#[derive(Debug, Clone, Copy)]
pub enum SessionAction {
    StartSession,
    IdleSession,
    ResumeSession,
}

impl SessionAction {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionAction::StartSession => "StartSession",
            SessionAction::IdleSession => "IdleSession",
            SessionAction::ResumeSession => "ResumeSession",
        }
    }
}

// ======================================================================
// Row structs (PascalCase on the wire)
// ======================================================================

/// Environment row. For Phase 4 we always create a `Local` /
/// `Unrestricted` environment (no MCP servers, no package managers)
/// because that combination passes the Phase 0 hard constraint with
/// the minimum number of required fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvironmentRow {
    #[serde(rename(serialize = "id", deserialize = "Id"))]
    pub id: String,
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Status")]
    pub status: String,
    #[serde(rename = "ConfigType")]
    pub config_type: String,
    #[serde(rename = "NetworkingType")]
    pub networking_type: String,
    #[serde(rename = "CreatedAt")]
    pub created_at: String,
    #[serde(rename = "UpdatedAt")]
    pub updated_at: String,
    #[serde(rename = "Description", skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "AllowMcpServers", skip_serializing_if = "Option::is_none")]
    pub allow_mcp_servers: Option<bool>,
    #[serde(
        rename = "AllowPackageManagers",
        skip_serializing_if = "Option::is_none"
    )]
    pub allow_package_managers: Option<bool>,
    #[serde(rename = "Metadata", skip_serializing_if = "Option::is_none")]
    pub metadata: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagedAgentRow {
    #[serde(rename(serialize = "id", deserialize = "Id"))]
    pub id: String,
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "ModelId")]
    pub model_id: String,
    #[serde(rename = "Status")]
    pub status: String,
    #[serde(rename = "Version")]
    pub version: i32,
    #[serde(rename = "CreatedAt")]
    pub created_at: String,
    #[serde(rename = "UpdatedAt")]
    pub updated_at: String,
    #[serde(rename = "Description", skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "System", skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    #[serde(rename = "ModelSpeed", skip_serializing_if = "Option::is_none")]
    pub model_speed: Option<String>,
    #[serde(rename = "Metadata", skip_serializing_if = "Option::is_none")]
    pub metadata: Option<String>,
    #[serde(rename = "ArchivedAt", skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRow {
    #[serde(rename(serialize = "id", deserialize = "Id"))]
    pub id: String,
    #[serde(rename = "AgentId")]
    pub agent_id: String,
    #[serde(rename = "EnvironmentId")]
    pub environment_id: String,
    #[serde(rename = "Status")]
    pub status: String,
    #[serde(rename = "CreatedAt")]
    pub created_at: String,
    #[serde(rename = "UpdatedAt")]
    pub updated_at: String,
    #[serde(rename = "AgentVersion", skip_serializing_if = "Option::is_none")]
    pub agent_version: Option<i32>,
    #[serde(rename = "Title", skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(rename = "Metadata", skip_serializing_if = "Option::is_none")]
    pub metadata: Option<String>,
    #[serde(rename = "ActiveSeconds", skip_serializing_if = "Option::is_none")]
    pub active_seconds: Option<f64>,
    #[serde(rename = "DurationSeconds", skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<f64>,
    #[serde(rename = "InputTokens", skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<i64>,
    #[serde(rename = "OutputTokens", skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<i64>,
    #[serde(
        rename = "CacheReadInputTokens",
        skip_serializing_if = "Option::is_none"
    )]
    pub cache_read_input_tokens: Option<i64>,
    #[serde(
        rename = "CacheCreation1hInputTokens",
        skip_serializing_if = "Option::is_none"
    )]
    pub cache_creation_1h_input_tokens: Option<i64>,
    #[serde(
        rename = "CacheCreation5mInputTokens",
        skip_serializing_if = "Option::is_none"
    )]
    pub cache_creation_5m_input_tokens: Option<i64>,
    #[serde(rename = "TerminatedAt", skip_serializing_if = "Option::is_none")]
    pub terminated_at: Option<String>,
    #[serde(rename = "ArchivedAt", skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<String>,
}

/// A SessionEvent row — the append-only 20-way discriminator table.
/// Only the columns Phase 4 actually reads or writes are declared;
/// the turn loop only emits five kinds (`user.message`,
/// `span.model_request_start`, `agent.message`, `span.model_request_end`,
/// `session.status_idle`) so most of the 20 extra columns ADR-0045
/// added stay as `None`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEventRow {
    #[serde(rename(serialize = "id", deserialize = "Id"))]
    pub id: String,
    #[serde(rename = "SessionId")]
    pub session_id: String,
    #[serde(rename = "Sequence")]
    pub sequence: i64,
    #[serde(rename = "Kind")]
    pub kind: String,
    #[serde(rename = "CreatedAt")]
    pub created_at: String,
    #[serde(rename = "ProcessedAt", skip_serializing_if = "Option::is_none")]
    pub processed_at: Option<String>,
    #[serde(rename = "Content", skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(rename = "StopReason", skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(
        rename = "StopReasonEventIds",
        skip_serializing_if = "Option::is_none"
    )]
    pub stop_reason_event_ids: Option<String>,
    #[serde(
        rename = "ModelRequestStartId",
        skip_serializing_if = "Option::is_none"
    )]
    pub model_request_start_id: Option<String>,
    #[serde(rename = "IsError", skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(rename = "ModelInputTokens", skip_serializing_if = "Option::is_none")]
    pub model_input_tokens: Option<i64>,
    #[serde(
        rename = "ModelOutputTokens",
        skip_serializing_if = "Option::is_none"
    )]
    pub model_output_tokens: Option<i64>,
    #[serde(
        rename = "ModelCacheCreationInputTokens",
        skip_serializing_if = "Option::is_none"
    )]
    pub model_cache_creation_input_tokens: Option<i64>,
    #[serde(
        rename = "ModelCacheReadInputTokens",
        skip_serializing_if = "Option::is_none"
    )]
    pub model_cache_read_input_tokens: Option<i64>,
    #[serde(rename = "ModelSpeed", skip_serializing_if = "Option::is_none")]
    pub model_speed: Option<String>,
    #[serde(rename = "ToolName", skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(rename = "ToolUseId", skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
}

/// An AgentTool row — child of ManagedAgent with a `Kind` discriminator.
/// Phase 6 only creates `agent_toolset` rows (no kind-specific fields).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentToolRow {
    #[serde(rename(serialize = "id", deserialize = "Id"))]
    pub id: String,
    #[serde(rename = "AgentId")]
    pub agent_id: String,
    #[serde(rename = "Kind")]
    pub kind: String,
}
