//! Webhook dispatch for Temper entity state transitions.
//!
//! After a successful action, any matching webhook configurations loaded from
//! `webhooks.toml` are fired asynchronously (fire-and-forget). Failures are
//! logged via `tracing` and never affect the action response.

use std::collections::BTreeMap;

use crate::state::TrajectoryEntry;

/// Configuration for a single webhook endpoint.
///
/// Parsed from a `[[webhook]]` section in `webhooks.toml`.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct WebhookConfig {
    /// Human-readable name used in log messages.
    pub name: String,
    /// Target URL for the POST request.
    pub url: String,
    /// HTTP headers to include. Values support `${ENV_VAR}` expansion at load time.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Fire only for these action names. Empty = all actions.
    #[serde(default)]
    pub actions: Vec<String>,
    /// Fire only for these entity types. Empty = all entity types.
    #[serde(default)]
    pub entity_types: Vec<String>,
    /// Fire only when the action succeeded. Default: `true`.
    #[serde(default = "default_true")]
    pub on_success_only: bool,
    /// Payload template sent as the POST body.
    ///
    /// Supports `${tenant}`, `${entity_type}`, `${entity_id}`, `${action}`,
    /// `${from_status}`, `${to_status}`.
    #[serde(default)]
    pub payload_template: String,
}

fn default_true() -> bool {
    true
}

/// Top-level TOML structure for `webhooks.toml`.
#[derive(Debug, serde::Deserialize)]
struct WebhooksFile {
    #[serde(rename = "webhook", default)]
    webhooks: Vec<WebhookConfig>,
}

/// Dispatches webhook HTTP POST calls after entity actions.
///
/// Built once at startup from `webhooks.toml`. The underlying
/// [`reqwest::Client`] pools connections, so `WebhookDispatcher` should be
/// shared via `Arc`.
pub struct WebhookDispatcher {
    client: reqwest::Client,
    configs: Vec<WebhookConfig>,
}

impl WebhookDispatcher {
    /// Create a dispatcher from a list of pre-parsed configs.
    pub fn new(configs: Vec<WebhookConfig>) -> Self {
        Self {
            client: reqwest::Client::new(),
            configs,
        }
    }

    /// Parse `webhooks.toml` TOML source into a dispatcher.
    ///
    /// Header values containing `${ENV_VAR}` are expanded from the environment
    /// at construction time so secrets are resolved once, not on every request.
    pub fn from_toml(source: &str) -> Result<Self, String> {
        let file: WebhooksFile =
            toml::from_str(source).map_err(|e| format!("failed to parse webhooks.toml: {e}"))?;

        let configs = file
            .webhooks
            .into_iter()
            .map(|mut cfg| {
                // determinism-ok: env var expansion runs once at startup, not per-request
                for value in cfg.headers.values_mut() {
                    *value = expand_env_vars(value);
                }
                cfg
            })
            .collect();

        Ok(Self::new(configs))
    }

    /// Read-only access to the loaded webhook configurations.
    pub fn configs(&self) -> &[WebhookConfig] {
        &self.configs
    }

    /// Dispatch webhooks matching the given trajectory entry.
    ///
    /// Spawns one independent async task per matching webhook. Returns
    /// immediately — the caller is never blocked by webhook latency or failure.
    pub fn dispatch(&self, entry: &TrajectoryEntry) {
        for config in &self.configs {
            if !self.matches(config, entry) {
                continue;
            }

            let payload = expand_template(&config.payload_template, entry);
            let client = self.client.clone();
            let url = config.url.clone();
            let name = config.name.clone();
            let headers = config.headers.clone();

            tokio::spawn(async move {
                // determinism-ok: fire-and-forget webhook side-effect; no simulation-visible state touched
                let mut builder = client
                    .post(&url)
                    .header("Content-Type", "application/json")
                    .body(payload);

                for (k, v) in &headers {
                    builder = builder.header(k.as_str(), v.as_str());
                }

                match builder.send().await {
                    Ok(resp) => {
                        let status = resp.status();
                        if status.is_success() {
                            tracing::debug!(
                                webhook = %name,
                                url = %url,
                                %status,
                                "webhook dispatched successfully"
                            );
                        } else {
                            tracing::warn!(
                                webhook = %name,
                                url = %url,
                                %status,
                                "webhook returned non-success status"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            webhook = %name,
                            url = %url,
                            error = %e,
                            "webhook dispatch failed"
                        );
                    }
                }
            });
        }
    }

    /// Returns `true` if the config should fire for the given trajectory entry.
    fn matches(&self, config: &WebhookConfig, entry: &TrajectoryEntry) -> bool {
        if config.on_success_only && !entry.success {
            return false;
        }
        if !config.actions.is_empty() && !config.actions.iter().any(|a| a == &entry.action) {
            return false;
        }
        if !config.entity_types.is_empty()
            && !config.entity_types.iter().any(|t| t == &entry.entity_type)
        {
            return false;
        }
        true
    }
}

/// Expand `${VAR_NAME}` patterns in `s` using environment variables.
///
/// Unknown variables are replaced with an empty string. Expansion is
/// non-recursive (the replacement value is never re-scanned).
fn expand_env_vars(s: &str) -> String {
    let mut result = s.to_string();
    let mut cursor = 0;
    while let Some(rel_start) = result[cursor..].find("${") {
        let abs_start = cursor + rel_start;
        let Some(rel_end) = result[abs_start..].find('}') else {
            break;
        };
        let abs_end = abs_start + rel_end;
        let var_name = result[abs_start + 2..abs_end].to_string();
        let replacement = std::env::var(&var_name).unwrap_or_default(); // determinism-ok: env var read once at startup, not per simulation step
        let prefix = result[..abs_start].to_string();
        let suffix = result[abs_end + 1..].to_string();
        result = format!("{prefix}{replacement}{suffix}");
        // Advance past the replacement to avoid infinite loops if replacement
        // itself contains `${` (edge case, but safe to skip).
        cursor = abs_start + replacement.len();
    }
    result
}

/// Expand `${variable}` placeholders using fields from a [`TrajectoryEntry`].
fn expand_template(template: &str, entry: &TrajectoryEntry) -> String {
    template
        .replace("${tenant}", &entry.tenant)
        .replace("${entity_type}", &entry.entity_type)
        .replace("${entity_id}", &entry.entity_id)
        .replace("${action}", &entry.action)
        .replace("${from_status}", entry.from_status.as_deref().unwrap_or(""))
        .replace("${to_status}", entry.to_status.as_deref().unwrap_or(""))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(
        action: &str,
        entity_type: &str,
        entity_id: &str,
        success: bool,
        from: Option<&str>,
        to: Option<&str>,
    ) -> TrajectoryEntry {
        TrajectoryEntry {
            timestamp: "2026-01-01T00:00:00Z".into(),
            tenant: "default".into(),
            entity_type: entity_type.into(),
            entity_id: entity_id.into(),
            action: action.into(),
            success,
            from_status: from.map(String::from),
            to_status: to.map(String::from),
            error: None,
            agent_id: None,
            session_id: None,
            authz_denied: None,
            denied_resource: None,
            denied_module: None,
            source: None,
            spec_governed: None,
        }
    }

    // ── Template expansion ────────────────────────────────────────────────────

    #[test]
    fn template_expansion_all_vars() {
        let tmpl =
            "[${tenant}] ${action} on ${entity_type}:${entity_id} (${from_status} -> ${to_status})";
        let e = entry(
            "Approve",
            "Proposal",
            "p-1",
            true,
            Some("Draft"),
            Some("Approved"),
        );
        assert_eq!(
            expand_template(tmpl, &e),
            "[default] Approve on Proposal:p-1 (Draft -> Approved)"
        );
    }

    #[test]
    fn template_expansion_missing_statuses_become_empty() {
        let e = entry("Select", "Proposal", "p-1", true, None, None);
        assert_eq!(expand_template("${from_status}|${to_status}", &e), "|");
    }

    #[test]
    fn template_expansion_no_placeholders() {
        let e = entry("Select", "Proposal", "p-1", true, None, None);
        assert_eq!(expand_template("static payload", &e), "static payload");
    }

    // ── Action filter ─────────────────────────────────────────────────────────

    #[test]
    fn action_filter_allows_listed_actions() {
        let toml = r#"
[[webhook]]
name = "test"
url = "http://example.com/hook"
actions = ["Approve", "Reject"]
payload_template = "test"
"#;
        let d = WebhookDispatcher::from_toml(toml).unwrap();
        let cfg = &d.configs[0];
        assert!(d.matches(cfg, &entry("Approve", "Proposal", "p-1", true, None, None)));
        assert!(d.matches(cfg, &entry("Reject", "Proposal", "p-1", true, None, None)));
        assert!(!d.matches(cfg, &entry("Select", "Proposal", "p-1", true, None, None)));
    }

    #[test]
    fn action_filter_empty_matches_all() {
        let toml = r#"
[[webhook]]
name = "test"
url = "http://example.com/hook"
payload_template = "test"
"#;
        let d = WebhookDispatcher::from_toml(toml).unwrap();
        let cfg = &d.configs[0];
        assert!(d.matches(cfg, &entry("AnyAction", "AnyType", "id", true, None, None)));
    }

    // ── Entity-type filter ────────────────────────────────────────────────────

    #[test]
    fn entity_type_filter_allows_listed_types() {
        let toml = r#"
[[webhook]]
name = "test"
url = "http://example.com/hook"
entity_types = ["Proposal"]
payload_template = "test"
"#;
        let d = WebhookDispatcher::from_toml(toml).unwrap();
        let cfg = &d.configs[0];
        assert!(d.matches(cfg, &entry("Select", "Proposal", "p-1", true, None, None)));
        assert!(!d.matches(cfg, &entry("Submit", "Order", "o-1", true, None, None)));
    }

    // ── on_success_only filter ────────────────────────────────────────────────

    #[test]
    fn on_success_only_rejects_failed_actions() {
        let toml = r#"
[[webhook]]
name = "test"
url = "http://example.com/hook"
on_success_only = true
payload_template = "test"
"#;
        let d = WebhookDispatcher::from_toml(toml).unwrap();
        let cfg = &d.configs[0];
        assert!(d.matches(cfg, &entry("Select", "Proposal", "p-1", true, None, None)));
        assert!(!d.matches(cfg, &entry("Select", "Proposal", "p-1", false, None, None)));
    }

    #[test]
    fn on_success_only_defaults_to_true() {
        let toml = r#"
[[webhook]]
name = "test"
url = "http://example.com/hook"
payload_template = "test"
"#;
        let d = WebhookDispatcher::from_toml(toml).unwrap();
        assert!(d.configs[0].on_success_only);
        let cfg = &d.configs[0];
        assert!(!d.matches(cfg, &entry("Select", "Proposal", "p-1", false, None, None)));
    }

    // ── TOML parsing ──────────────────────────────────────────────────────────

    #[test]
    fn parse_multiple_webhooks() {
        let toml = r#"
[[webhook]]
name = "first"
url = "http://example.com/first"
payload_template = "first"

[[webhook]]
name = "second"
url = "http://example.com/second"
payload_template = "second"
"#;
        let d = WebhookDispatcher::from_toml(toml).unwrap();
        assert_eq!(d.configs.len(), 2);
        assert_eq!(d.configs[0].name, "first");
        assert_eq!(d.configs[1].name, "second");
    }

    #[test]
    fn parse_empty_webhooks_file() {
        let d = WebhookDispatcher::from_toml("").unwrap();
        assert!(d.configs.is_empty());
    }

    // ── Env-var header expansion ──────────────────────────────────────────────

    #[test]
    fn env_var_expansion_in_headers() {
        // SAFETY: test-only; safe in a single-threaded test context.
        unsafe { std::env::set_var("TEMPER_TEST_TOKEN_WEBHOOK", "secret-abc") };
        let toml = r#"
[[webhook]]
name = "test"
url = "http://example.com/hook"
payload_template = "test"
headers = { Authorization = "Bearer ${TEMPER_TEST_TOKEN_WEBHOOK}" }
"#;
        let d = WebhookDispatcher::from_toml(toml).unwrap();
        let auth = d.configs[0].headers.get("Authorization").unwrap();
        assert_eq!(auth, "Bearer secret-abc");
        unsafe { std::env::remove_var("TEMPER_TEST_TOKEN_WEBHOOK") };
    }

    #[test]
    fn env_var_expansion_unknown_var_becomes_empty() {
        assert_eq!(
            expand_env_vars("prefix-${TEMPER_DEFINITELY_UNSET_XYZ_123}-suffix"),
            "prefix--suffix"
        );
    }

    // ── Integration: mock HTTP server ─────────────────────────────────────────

    #[tokio::test]
    async fn integration_webhook_reaches_mock_server() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&mock_server)
            .await;

        let toml = format!(
            r#"
[[webhook]]
name = "test"
url = "{}/hook"
payload_template = "{{\"action\": \"${{action}}\"}}"
"#,
            mock_server.uri()
        );
        let d = WebhookDispatcher::from_toml(&toml).unwrap();
        d.dispatch(&entry(
            "Approve",
            "Proposal",
            "p-1",
            true,
            Some("Draft"),
            Some("Approved"),
        ));

        // Give the spawned task a moment to complete.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await; // determinism-ok: test-only helper delay
        mock_server.verify().await;
    }

    #[tokio::test]
    async fn integration_failed_action_not_dispatched_by_default() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0) // must NOT receive any call
            .mount(&mock_server)
            .await;

        let toml = format!(
            r#"
[[webhook]]
name = "test"
url = "{}/hook"
payload_template = "test"
"#,
            mock_server.uri()
        );
        let d = WebhookDispatcher::from_toml(&toml).unwrap();
        // success = false → on_success_only (default true) should suppress
        d.dispatch(&entry("Select", "Proposal", "p-1", false, None, None));

        tokio::time::sleep(std::time::Duration::from_millis(100)).await; // determinism-ok: test-only helper delay
        mock_server.verify().await;
    }

    #[tokio::test]
    async fn integration_webhook_failure_does_not_panic() {
        // Point to a port that is not listening — dispatch should silently fail.
        let toml = r#"
[[webhook]]
name = "test"
url = "http://127.0.0.1:19997/hook"
payload_template = "test"
"#;
        let d = WebhookDispatcher::from_toml(toml).unwrap();
        d.dispatch(&entry(
            "Approve",
            "Proposal",
            "p-1",
            true,
            Some("Draft"),
            Some("Approved"),
        ));
        // Wait for the spawned task to fail gracefully — no panic expected.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await; // determinism-ok: test-only helper delay
    }
}
