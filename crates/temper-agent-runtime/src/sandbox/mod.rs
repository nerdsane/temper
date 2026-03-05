//! Embedded Monty sandbox for agent code execution.
//!
//! The agent's single tool is `execute_code` — run Python in the sandbox.
//! Entity operations go through `temper.*` (HTTP to server via temper-sandbox).
//! Local tools (bash, file I/O) go through a governed `tools.*` namespace
//! with Cedar authorization first, then local execution.

pub mod dispatch;

use anyhow::Result;
use monty::MontyObject;
use serde_json::Value;

/// Embedded Monty sandbox for agent Python execution.
///
/// Provides two dispatch namespaces:
/// - `temper.*` → HTTP to Temper server (entity CRUD, governance, evolution, WASM, navigation)
/// - `tools.*` → Cedar-gated local execution (bash, file I/O)
#[derive(Clone)]
pub struct AgentSandbox {
    /// HTTP client for server communication.
    pub(crate) http: reqwest::Client,
    /// Base URL of the Temper server (e.g., `http://127.0.0.1:3000`).
    pub(crate) server_url: String,
    /// Tenant name.
    pub(crate) tenant: String,
    /// Agent principal ID for Cedar authorization.
    pub(crate) principal_id: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    /// Optional governance context (event callback + inline resolver).
    pub(crate) governance: Option<dispatch::GovernanceContext>,
}

impl AgentSandbox {
    /// Create a new sandbox connected to a Temper server.
    pub fn new(server_url: &str, tenant: &str, principal_id: Option<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            server_url: server_url.to_string(),
            tenant: tenant.to_string(),
            principal_id: std::sync::Arc::new(std::sync::Mutex::new(principal_id)),
            governance: None,
        }
    }

    /// Set the governance context (event callback + optional inline resolver).
    pub fn with_governance(mut self, ctx: dispatch::GovernanceContext) -> Self {
        self.governance = Some(ctx);
        self
    }

    /// Set the agent principal ID (call after agent entity is created).
    pub fn set_principal_id(&self, id: String) {
        *self.principal_id.lock().unwrap() = Some(id); // ci-ok: infallible lock
    }

    /// Get a clone of the principal ID Arc for sharing.
    pub fn principal_id_handle(&self) -> std::sync::Arc<std::sync::Mutex<Option<String>>> {
        self.principal_id.clone()
    }

    /// Execute Python code in the sandbox.
    ///
    /// The code can use:
    /// - `temper.*` methods for entity operations (HTTP to server)
    /// - `tools.*` methods for local operations (Cedar-gated)
    pub async fn run_code(&self, code: &str) -> Result<String> {
        let http = self.http.clone();
        let server_url = self.server_url.clone();
        let tenant = self.tenant.clone();
        let principal_id_arc = self.principal_id.clone();
        let governance = self.governance.clone();

        temper_sandbox::runner::run_sandbox(
            code,
            "agent.py",
            &[("temper", "Temper", 1), ("tools", "Tools", 2)],
            |function_name: String,
             args: Vec<MontyObject>,
             kwargs: Vec<(MontyObject, MontyObject)>| {
                let http = http.clone();
                let server_url = server_url.clone();
                let tenant = tenant.clone();
                let principal_id_arc = principal_id_arc.clone();
                let governance = governance.clone();
                async move {
                    dispatch_method(
                        &http,
                        &server_url,
                        &tenant,
                        &principal_id_arc,
                        &function_name,
                        &args,
                        &kwargs,
                        governance.as_ref(),
                    )
                    .await
                }
            },
        )
        .await
    }
}

/// Route a method call to the appropriate namespace.
#[allow(clippy::too_many_arguments)]
async fn dispatch_method(
    http: &reqwest::Client,
    server_url: &str,
    tenant: &str,
    principal_id: &std::sync::Arc<std::sync::Mutex<Option<String>>>,
    function_name: &str,
    args: &[MontyObject],
    kwargs: &[(MontyObject, MontyObject)],
    governance: Option<&dispatch::GovernanceContext>,
) -> Result<Value, String> {
    // Determine namespace from self (args[0]) Dataclass name.
    let namespace = args
        .first()
        .and_then(|a| match a {
            MontyObject::Dataclass { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .unwrap_or("unknown");

    match namespace {
        "Temper" => {
            // Strip self arg
            let args = if args.is_empty() { args } else { &args[1..] };
            let pid = {
                principal_id.lock().unwrap().clone() // ci-ok: infallible lock
            };
            let result = temper_sandbox::dispatch::dispatch_temper_method(
                http,
                server_url,
                tenant,
                pid.as_deref(),
                function_name,
                args,
                kwargs,
                None, // No entity set resolver for agent (uses entity type directly)
                None, // No binary path for agent
            )
            .await?;

            // Intercept Cedar denials and run governance resolver.
            if result.get("denied").and_then(Value::as_bool) == Some(true)
                && let Some(ctx) = governance
            {
                let decision_id = result
                    .get("pending_decision")
                    .or_else(|| result.get("decision_id"))
                    .and_then(Value::as_str)
                    .unwrap_or("");

                if !decision_id.is_empty() {
                    // Wait for approval via inline prompt + polling.
                    dispatch::resolve_temper_denial(
                        http,
                        server_url,
                        tenant,
                        decision_id,
                        &format!("temper.{function_name}"),
                        function_name,
                        "SpecSubmission",
                        ctx,
                    )
                    .await?;

                    // Approved — retry the original call.
                    return temper_sandbox::dispatch::dispatch_temper_method(
                        http,
                        server_url,
                        tenant,
                        pid.as_deref(),
                        function_name,
                        args,
                        kwargs,
                        None,
                        None,
                    )
                    .await;
                }
            }

            Ok(result)
        }
        "Tools" => {
            dispatch::dispatch_tools_method(
                http,
                server_url,
                tenant,
                principal_id,
                function_name,
                args,
                governance,
            )
            .await
        }
        _ => Err(format!(
            "unknown namespace '{namespace}' for method '{function_name}'. \
             Use temper.<method> or tools.<method>."
        )),
    }
}

#[cfg(test)]
mod tests {
    use temper_sandbox::helpers::wrap_user_code;

    #[test]
    fn test_wrap_user_code_basic() {
        let wrapped = wrap_user_code("x = 1\nreturn x");
        assert!(wrapped.contains("async def __temper_user():"));
        assert!(wrapped.contains("    x = 1"));
        assert!(wrapped.contains("    return x"));
        assert!(wrapped.contains("await __temper_user()"));
    }

    #[test]
    fn test_wrap_user_code_empty() {
        let wrapped = wrap_user_code("");
        assert!(wrapped.contains("    return None"));
    }
}
