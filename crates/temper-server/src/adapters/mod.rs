//! Native agent adapter integrations for `type = "adapter"` execution.
//!
//! Adapters run in platform Rust code (not WASM), enabling capabilities like
//! CLI process execution and WebSocket gateway sessions while preserving
//! IOA-declared integration intent.

mod claude_code;
mod codex;
mod http_webhook;
mod openclaw;

use std::collections::BTreeMap;
use std::env::var_os as read_process_environment; // determinism-ok: external CLI launch boundary
use std::fmt;
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncReadExt};

pub use claude_code::ClaudeCodeAdapter;
pub use codex::CodexAdapter;
pub use http_webhook::HttpWebhookAdapter;
pub use openclaw::OpenClawAdapter;

/// Agent identity context provided to adapter executions.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct AdapterAgentContext {
    /// Calling principal ID.
    pub agent_id: Option<String>,
    /// Calling session identifier.
    pub session_id: Option<String>,
    /// Calling agent type classification.
    pub agent_type: Option<String>,
    /// Platform-minted API key for credential-based identity resolution.
    ///
    /// Set by the adapter dispatch flow when the entity has an `agent_type_id`.
    /// The adapter passes this to the spawned process via `TEMPER_API_KEY`.
    /// Never persisted — exists only for the lifetime of the adapter invocation.
    #[serde(skip)]
    pub agent_api_key: Option<String>,
}

impl fmt::Debug for AdapterAgentContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AdapterAgentContext")
            .field("agent_id", &self.agent_id)
            .field("session_id", &self.session_id)
            .field("agent_type", &self.agent_type)
            .field("agent_api_key_present", &self.agent_api_key.is_some())
            .finish()
    }
}

/// Full adapter invocation context built from dispatch state.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct AdapterContext {
    /// Tenant identifier.
    pub tenant: String,
    /// Entity type being dispatched.
    pub entity_type: String,
    /// Entity ID being dispatched.
    pub entity_id: String,
    /// Trigger action name.
    pub trigger_action: String,
    /// Trigger action parameters.
    pub trigger_params: serde_json::Value,
    /// Serialized current entity state.
    pub entity_state: serde_json::Value,
    /// Integration config with secret templates resolved.
    #[serde(skip_serializing, default)]
    pub integration_config: BTreeMap<String, String>,
    /// Agent identity context.
    pub agent_ctx: AdapterAgentContext,
    /// Per-tenant secrets snapshot for adapter use.
    #[serde(skip_serializing, default)]
    pub secrets: BTreeMap<String, String>,
}

impl fmt::Debug for AdapterContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AdapterContext")
            .field("tenant", &self.tenant)
            .field("entity_type", &self.entity_type)
            .field("entity_id", &self.entity_id)
            .field("trigger_action", &self.trigger_action)
            .field("agent_ctx", &self.agent_ctx)
            .field("integration_config_count", &self.integration_config.len())
            .field("secret_count", &self.secrets.len())
            .finish()
    }
}

impl AdapterContext {
    /// Retrieve a secret value by key from the invocation snapshot.
    pub fn get_secret(&self, key: &str) -> Option<String> {
        self.secrets.get(key).cloned()
    }
}

/// Adapter invocation result.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AdapterResult {
    /// Optional callback action suggested by the adapter implementation.
    pub callback_action: Option<String>,
    /// Callback params produced by the adapter.
    pub callback_params: serde_json::Value,
    /// Whether adapter execution succeeded.
    pub success: bool,
    /// Optional failure description when `success` is false.
    pub error: Option<String>,
    /// End-to-end adapter runtime duration.
    pub duration_ms: u64,
}

impl AdapterResult {
    /// Build a successful adapter result.
    pub fn success(callback_params: serde_json::Value, duration_ms: u64) -> Self {
        Self {
            callback_action: None,
            callback_params,
            success: true,
            error: None,
            duration_ms,
        }
    }

    /// Build a failed adapter result.
    pub fn failure(error: String, duration_ms: u64) -> Self {
        Self {
            callback_action: None,
            callback_params: serde_json::json!({}),
            success: false,
            error: Some(error),
            duration_ms,
        }
    }
}

/// Typed adapter execution errors.
#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    /// Adapter invocation could not be started.
    #[error("adapter invocation failed: {0}")]
    Invocation(String),
    /// Adapter execution failed with runtime error.
    #[error("adapter execution failed: {0}")]
    Execution(String),
    /// Adapter output could not be parsed.
    #[error("adapter output parse failed: {0}")]
    Parse(String),
}

/// Trait implemented by all native adapter integrations.
#[async_trait]
pub trait AgentAdapter: Send + Sync {
    /// Stable adapter type key used for registry lookup.
    fn adapter_type(&self) -> &str;

    /// Whether this adapter launches a client that must authenticate back to
    /// Temper during the invocation.
    fn requires_platform_credential(&self) -> bool {
        false
    }

    /// Execute this adapter with the provided invocation context.
    async fn execute(&self, ctx: AdapterContext) -> Result<AdapterResult, AdapterError>;
}

fn configure_temper_api_key(command: &mut tokio::process::Command, api_key: Option<&str>) {
    // Defense in depth for callers that do not use the complete CLI boundary.
    command.env_remove("TEMPER_API_KEY");
    if let Some(api_key) = api_key {
        command.env("TEMPER_API_KEY", api_key);
    }
}

#[derive(Clone, Copy)]
pub(super) enum CliAdapterEnvironment {
    ClaudeCode,
    Codex,
}

const CLI_BASE_ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "TMPDIR",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TERM",
    "COLORTERM",
    "NO_COLOR",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NIX_SSL_CERT_FILE",
];

const CLI_OUTPUT_STREAM_BUDGET_BYTES: usize = 4 * 1024 * 1024;

pub(super) struct CliProcessOutput {
    pub(super) status: ExitStatus,
    pub(super) stdout: Vec<u8>,
    pub(super) stderr: Vec<u8>,
}

struct BoundedStreamOutput {
    bytes: Vec<u8>,
    exceeded_budget: bool,
}

async fn read_bounded_cli_stream(
    mut stream: impl AsyncRead + Unpin,
) -> std::io::Result<BoundedStreamOutput> {
    let mut bytes = Vec::with_capacity(16 * 1024);
    let mut exceeded_budget = false;
    let mut chunk = [0_u8; 16 * 1024];
    loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        if !exceeded_budget
            && bytes
                .len()
                .checked_add(read)
                .is_some_and(|total| total <= CLI_OUTPUT_STREAM_BUDGET_BYTES)
        {
            bytes.extend_from_slice(&chunk[..read]);
        } else {
            exceeded_budget = true;
            break;
        }
    }
    Ok(BoundedStreamOutput {
        bytes,
        exceeded_budget,
    })
}

/// Execute one CLI adapter child while bounding captured stdout and stderr.
///
/// A reader signals as soon as its retention budget is exhausted; the parent is
/// then killed and reaped. Concurrent reads prevent either pipe from blocking a
/// well-behaved child while it exits.
pub(super) async fn execute_cli_command(
    command: &mut tokio::process::Command,
    command_name: &str,
) -> Result<CliProcessOutput, AdapterError> {
    command
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|error| {
        AdapterError::Invocation(format!("failed to spawn '{command_name}': {error}"))
    })?;
    let stdout = child.stdout.take().ok_or_else(|| {
        AdapterError::Invocation(format!("failed to capture '{command_name}' stdout"))
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        AdapterError::Invocation(format!("failed to capture '{command_name}' stderr"))
    })?;

    enum Completion {
        Complete(CliProcessOutput),
        Failed(AdapterError),
    }

    let completion = {
        let mut wait = Box::pin(child.wait());
        let mut read_stdout = Box::pin(read_bounded_cli_stream(stdout));
        let mut read_stderr = Box::pin(read_bounded_cli_stream(stderr));
        let mut status = None;
        let mut captured_stdout = None;
        let mut captured_stderr = None;

        loop {
            tokio::select! {
                result = &mut wait, if status.is_none() => {
                    match result {
                        Ok(value) => status = Some(value),
                        Err(error) => break Completion::Failed(AdapterError::Execution(format!(
                            "failed waiting for '{command_name}': {error}"
                        ))),
                    }
                }
                result = &mut read_stdout, if captured_stdout.is_none() => {
                    match result {
                        Ok(output) if output.exceeded_budget => {
                            break Completion::Failed(AdapterError::Execution(format!(
                                "'{command_name}' stdout exceeded the {CLI_OUTPUT_STREAM_BUDGET_BYTES}-byte budget"
                            )));
                        }
                        Ok(output) => captured_stdout = Some(output.bytes),
                        Err(error) => break Completion::Failed(AdapterError::Execution(format!(
                            "failed reading '{command_name}' stdout: {error}"
                        ))),
                    }
                }
                result = &mut read_stderr, if captured_stderr.is_none() => {
                    match result {
                        Ok(output) if output.exceeded_budget => {
                            break Completion::Failed(AdapterError::Execution(format!(
                                "'{command_name}' stderr exceeded the {CLI_OUTPUT_STREAM_BUDGET_BYTES}-byte budget"
                            )));
                        }
                        Ok(output) => captured_stderr = Some(output.bytes),
                        Err(error) => break Completion::Failed(AdapterError::Execution(format!(
                            "failed reading '{command_name}' stderr: {error}"
                        ))),
                    }
                }
            }

            if status.is_some() && captured_stdout.is_some() && captured_stderr.is_some() {
                match (
                    status.take(),
                    captured_stdout.take(),
                    captured_stderr.take(),
                ) {
                    (Some(status), Some(stdout), Some(stderr)) => {
                        break Completion::Complete(CliProcessOutput {
                            status,
                            stdout,
                            stderr,
                        });
                    }
                    _ => continue,
                }
            }
        }
    };

    match completion {
        Completion::Complete(output) => Ok(output),
        Completion::Failed(error) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            Err(error)
        }
    }
}

/// Install the complete environment for a tool-executing CLI child.
///
/// The child starts from an empty environment. Only non-authority runtime
/// variables, an explicitly configured home/config path, the selected
/// provider's tenant-scoped credentials, and the invocation bearer are added.
/// Database, Turso, deployment, webhook, unrelated provider, and proxy
/// credentials can therefore never cross this boundary by ambient inheritance.
pub(super) fn configure_cli_child_environment(
    command: &mut tokio::process::Command,
    ctx: &AdapterContext,
    adapter: CliAdapterEnvironment,
) {
    command.env_clear();
    for key in CLI_BASE_ENV_ALLOWLIST {
        if let Some(value) = read_process_environment(key) {
            command.env(key, value);
        }
    }

    install_config_env(command, ctx, "home", "HOME");
    match adapter {
        CliAdapterEnvironment::ClaudeCode => {
            install_config_env(command, ctx, "claude_config_dir", "CLAUDE_CONFIG_DIR");
            install_config_env(command, ctx, "anthropic_base_url", "ANTHROPIC_BASE_URL");
            install_tenant_secret(
                command,
                ctx,
                "ANTHROPIC_API_KEY",
                &["ANTHROPIC_API_KEY", "anthropic_api_key"],
            );
            install_tenant_secret(
                command,
                ctx,
                "ANTHROPIC_AUTH_TOKEN",
                &["ANTHROPIC_AUTH_TOKEN", "anthropic_auth_token"],
            );
            install_tenant_secret(
                command,
                ctx,
                "CLAUDE_CODE_OAUTH_TOKEN",
                &["CLAUDE_CODE_OAUTH_TOKEN", "claude_code_oauth_token"],
            );
        }
        CliAdapterEnvironment::Codex => {
            install_config_env(command, ctx, "codex_home", "CODEX_HOME");
            install_config_env(command, ctx, "openai_base_url", "OPENAI_BASE_URL");
            install_config_env(command, ctx, "openai_organization", "OPENAI_ORGANIZATION");
            install_config_env(command, ctx, "openai_project", "OPENAI_PROJECT");
            install_tenant_secret(
                command,
                ctx,
                "OPENAI_API_KEY",
                &["OPENAI_API_KEY", "openai_api_key"],
            );
        }
    }
    configure_temper_api_key(command, ctx.agent_ctx.agent_api_key.as_deref());
}

fn install_config_env(
    command: &mut tokio::process::Command,
    ctx: &AdapterContext,
    config_key: &str,
    env_key: &str,
) {
    if let Some(value) = ctx
        .integration_config
        .get(config_key)
        .filter(|value| !value.trim().is_empty())
    {
        command.env(env_key, value);
    }
}

fn install_tenant_secret(
    command: &mut tokio::process::Command,
    ctx: &AdapterContext,
    env_key: &str,
    secret_keys: &[&str],
) {
    if let Some(value) = secret_keys
        .iter()
        .find_map(|key| ctx.secrets.get(*key))
        .filter(|value| !value.is_empty())
    {
        command.env(env_key, value);
    }
}

/// Registry of available adapter implementations keyed by adapter type.
#[derive(Clone, Default)]
pub struct AdapterRegistry {
    /// Registered adapter implementations.
    adapters: BTreeMap<String, Arc<dyn AgentAdapter>>,
}

impl AdapterRegistry {
    /// Create an empty adapter registry.
    pub fn new() -> Self {
        Self {
            adapters: BTreeMap::new(),
        }
    }

    /// Create a registry with built-in adapter implementations registered.
    pub fn with_builtins() -> Self {
        let mut registry = Self::new();
        registry.register(Arc::new(ClaudeCodeAdapter));
        registry.register(Arc::new(CodexAdapter));
        registry.register(Arc::new(OpenClawAdapter));
        registry.register(Arc::new(HttpWebhookAdapter));
        registry
    }

    /// Register an adapter implementation.
    pub fn register(&mut self, adapter: Arc<dyn AgentAdapter>) {
        self.adapters
            .insert(adapter.adapter_type().to_string(), adapter);
    }

    /// Resolve an adapter by type key.
    pub fn get(&self, adapter_type: &str) -> Option<Arc<dyn AgentAdapter>> {
        self.adapters.get(adapter_type).cloned()
    }

    /// Return all registered adapter type keys in deterministic order.
    pub fn adapter_types(&self) -> Vec<String> {
        self.adapters.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AdapterAgentContext, AdapterContext, AdapterRegistry, CLI_OUTPUT_STREAM_BUDGET_BYTES,
        CliAdapterEnvironment, configure_cli_child_environment, configure_temper_api_key,
        execute_cli_command,
    };
    use std::collections::BTreeMap;

    #[test]
    fn builtins_are_registered() {
        let registry = AdapterRegistry::with_builtins();
        let adapter_types = registry.adapter_types();
        assert!(adapter_types.contains(&"claude_code".to_string()));
        assert!(adapter_types.contains(&"codex".to_string()));
        assert!(adapter_types.contains(&"openclaw".to_string()));
        assert!(adapter_types.contains(&"http".to_string()));
    }

    #[test]
    fn lookup_returns_registered_adapter() {
        let registry = AdapterRegistry::with_builtins();
        assert!(registry.get("claude_code").is_some());
        assert!(registry.get("codex").is_some());
        assert!(registry.get("openclaw").is_some());
        assert!(registry.get("http").is_some());
        assert!(registry.get("missing").is_none());
    }

    #[test]
    fn only_cli_adapters_request_platform_credentials() {
        let registry = AdapterRegistry::with_builtins();
        assert!(
            registry
                .get("claude_code")
                .expect("Claude adapter should exist")
                .requires_platform_credential()
        );
        assert!(
            registry
                .get("codex")
                .expect("Codex adapter should exist")
                .requires_platform_credential()
        );
        assert!(
            !registry
                .get("http")
                .expect("HTTP adapter should exist")
                .requires_platform_credential()
        );
        assert!(
            !registry
                .get("openclaw")
                .expect("OpenClaw adapter should exist")
                .requires_platform_credential()
        );
    }

    #[test]
    fn adapter_context_debug_redacts_plaintext_credential() {
        let context = AdapterContext {
            tenant: "tenant-a".to_string(),
            entity_type: "Run".to_string(),
            entity_id: "run-1".to_string(),
            trigger_action: "Execute".to_string(),
            trigger_params: serde_json::json!({"prompt": "trigger-secret"}),
            entity_state: serde_json::json!({"fields": {"secret": "state-secret"}}),
            integration_config: BTreeMap::from([(
                "authorization".to_string(),
                "integration-secret".to_string(),
            )]),
            agent_ctx: AdapterAgentContext {
                agent_api_key: Some("tmpr_super-secret".to_string()),
                ..AdapterAgentContext::default()
            },
            secrets: BTreeMap::from([(
                "ANTHROPIC_API_KEY".to_string(),
                "provider-secret".to_string(),
            )]),
        };
        let debug = format!("{context:?}");
        for secret in [
            "tmpr_super-secret",
            "trigger-secret",
            "state-secret",
            "integration-secret",
            "provider-secret",
        ] {
            assert!(!debug.contains(secret));
        }
        assert!(debug.contains("agent_api_key_present: true"));

        let serialized = serde_json::to_string(&context).expect("serialize redacted context");
        assert!(!serialized.contains("tmpr_super-secret"));
        assert!(!serialized.contains("integration-secret"));
        assert!(!serialized.contains("provider-secret"));
    }

    #[test]
    fn child_command_never_inherits_deployment_key() {
        let mut command = tokio::process::Command::new("unused");
        command.env("TEMPER_API_KEY", "deployment-root");
        configure_temper_api_key(&mut command, None);
        let without_lease = command
            .as_std()
            .get_envs()
            .find(|(key, _)| *key == "TEMPER_API_KEY")
            .expect("TEMPER_API_KEY should have an explicit removal entry");
        assert!(without_lease.1.is_none());

        configure_temper_api_key(&mut command, Some("tmpr_invocation-only"));
        let with_lease = command
            .as_std()
            .get_envs()
            .find(|(key, _)| *key == "TEMPER_API_KEY")
            .and_then(|(_, value)| value)
            .expect("invocation credential should be installed");
        assert_eq!(with_lease, "tmpr_invocation-only");
    }

    fn cli_environment_context() -> AdapterContext {
        AdapterContext {
            tenant: "tenant-a".to_string(),
            entity_type: "Run".to_string(),
            entity_id: "run-1".to_string(),
            trigger_action: "Execute".to_string(),
            trigger_params: serde_json::json!({}),
            entity_state: serde_json::json!({}),
            integration_config: BTreeMap::from([
                ("home".to_string(), "/tenant/home".to_string()),
                (
                    "claude_config_dir".to_string(),
                    "/tenant/claude".to_string(),
                ),
                ("codex_home".to_string(), "/tenant/codex".to_string()),
            ]),
            agent_ctx: AdapterAgentContext {
                agent_api_key: Some("tmpr_invocation-only".to_string()),
                ..AdapterAgentContext::default()
            },
            secrets: BTreeMap::from([
                (
                    "ANTHROPIC_API_KEY".to_string(),
                    "tenant-anthropic".to_string(),
                ),
                ("OPENAI_API_KEY".to_string(), "tenant-openai".to_string()),
                (
                    "unrelated_tenant_secret".to_string(),
                    "must-not-cross".to_string(),
                ),
            ]),
        }
    }

    fn plant_server_authority_sentinels(command: &mut tokio::process::Command) {
        for (key, value) in [
            ("DATABASE_URL", "database-root"),
            ("TURSO_AUTH_TOKEN", "turso-root"),
            ("TURSO_PLATFORM_AUTH_TOKEN", "turso-platform-root"),
            ("WEBHOOK_HMAC_SECRET", "webhook-root"),
            ("AWS_SECRET_ACCESS_KEY", "aws-root"),
            ("TEMPER_API_KEY", "deployment-root"),
        ] {
            command.env(key, value);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn claude_child_gets_only_explicit_selected_environment() {
        let mut command = tokio::process::Command::new("/bin/sh");
        plant_server_authority_sentinels(&mut command);
        configure_cli_child_environment(
            &mut command,
            &cli_environment_context(),
            CliAdapterEnvironment::ClaudeCode,
        );
        command.arg("-c").arg(
            r#"
            test -z "${DATABASE_URL-}"
            test -z "${TURSO_AUTH_TOKEN-}"
            test -z "${TURSO_PLATFORM_AUTH_TOKEN-}"
            test -z "${WEBHOOK_HMAC_SECRET-}"
            test -z "${AWS_SECRET_ACCESS_KEY-}"
            test -z "${OPENAI_API_KEY-}"
            test -z "${unrelated_tenant_secret-}"
            test "${TEMPER_API_KEY-}" = "tmpr_invocation-only"
            test "${ANTHROPIC_API_KEY-}" = "tenant-anthropic"
            test "${HOME-}" = "/tenant/home"
            test "${CLAUDE_CONFIG_DIR-}" = "/tenant/claude"
            test -n "${PATH-}"
            "#,
        );
        let output = command.output().await.expect("run isolated Claude child");
        assert!(
            output.status.success(),
            "isolated Claude environment assertion failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn codex_child_does_not_receive_claude_or_server_authority() {
        let mut command = tokio::process::Command::new("/bin/sh");
        plant_server_authority_sentinels(&mut command);
        configure_cli_child_environment(
            &mut command,
            &cli_environment_context(),
            CliAdapterEnvironment::Codex,
        );
        command.arg("-c").arg(
            r#"
            test -z "${DATABASE_URL-}"
            test -z "${TURSO_AUTH_TOKEN-}"
            test -z "${ANTHROPIC_API_KEY-}"
            test -z "${unrelated_tenant_secret-}"
            test "${TEMPER_API_KEY-}" = "tmpr_invocation-only"
            test "${OPENAI_API_KEY-}" = "tenant-openai"
            test "${HOME-}" = "/tenant/home"
            test "${CODEX_HOME-}" = "/tenant/codex"
            test -n "${PATH-}"
            "#,
        );
        let output = command.output().await.expect("run isolated Codex child");
        assert!(
            output.status.success(),
            "isolated Codex environment assertion failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cli_output_capture_rejects_streams_over_budget() {
        let mut command = tokio::process::Command::new("/usr/bin/head");
        command
            .arg("-c")
            .arg((CLI_OUTPUT_STREAM_BUDGET_BYTES + 1).to_string())
            .arg("/dev/zero")
            .kill_on_drop(true);

        let error = match execute_cli_command(&mut command, "bounded-output-test").await {
            Ok(_) => panic!("oversized stdout must fail closed"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("stdout exceeded"), "{error}");
    }
}
