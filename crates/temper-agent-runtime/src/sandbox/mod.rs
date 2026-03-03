//! Embedded Monty sandbox for agent code execution.
//!
//! The agent's single tool is `execute_code` — run Python in the sandbox.
//! Entity operations go through `temper.*` (HTTP to server).
//! Local tools (bash, file I/O) go through a governed `tools.*` namespace
//! with Cedar authorization first, then local execution.

mod convert;
pub(crate) mod dispatch;
pub(crate) mod helpers;

use std::collections::BTreeMap;

use anyhow::{Context, Result, anyhow, bail};
use monty::{
    DictPairs, ExcType, ExternalResult, LimitedTracker, MontyException, MontyObject, MontyRun,
    PrintWriter, RunProgress,
};
use serde_json::Value;

use self::convert::{json_to_monty_object, monty_object_to_json};
use self::helpers::{default_limits, format_monty_exception, wrap_user_code};

/// Embedded Monty sandbox for agent Python execution.
///
/// Provides two dispatch namespaces:
/// - `temper.*` → HTTP to Temper server (entity CRUD, governance)
/// - `tools.*` → Cedar-gated local execution (bash, file I/O)
pub struct AgentSandbox {
    /// HTTP client for server communication.
    pub(crate) http: reqwest::Client,
    /// Base URL of the Temper server (e.g., `http://127.0.0.1:3000`).
    pub(crate) server_url: String,
    /// Tenant name.
    pub(crate) tenant: String,
    /// Agent principal ID for Cedar authorization.
    pub(crate) principal_id: Option<String>,
}

impl AgentSandbox {
    /// Create a new sandbox connected to a Temper server.
    pub fn new(server_url: &str, tenant: &str, principal_id: Option<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            server_url: server_url.to_string(),
            tenant: tenant.to_string(),
            principal_id,
        }
    }

    /// Execute Python code in the sandbox.
    ///
    /// The code can use:
    /// - `temper.*` methods for entity operations (HTTP to server)
    /// - `tools.*` methods for local operations (Cedar-gated)
    pub async fn run_code(&self, code: &str) -> Result<String> {
        let program = wrap_user_code(code);
        let runner = MontyRun::new(
            program,
            "agent.py",
            vec!["temper".to_string(), "tools".to_string()],
            vec![],
        )
        .map_err(|e| anyhow!(format_monty_exception(&e)))?;

        let temper_object = MontyObject::Dataclass {
            name: "Temper".to_string(),
            type_id: 1,
            field_names: vec![],
            attrs: DictPairs::from(Vec::<(MontyObject, MontyObject)>::new()),
            frozen: true,
        };

        let tools_object = MontyObject::Dataclass {
            name: "Tools".to_string(),
            type_id: 2,
            field_names: vec![],
            attrs: DictPairs::from(Vec::<(MontyObject, MontyObject)>::new()),
            frozen: true,
        };

        let tracker = LimitedTracker::new(default_limits());

        // Start the Monty program. PrintWriter is created in its own scope
        // so it's dropped before any async work, keeping the future Send.
        let mut progress = {
            let mut print = PrintWriter::Disabled;
            runner
                .start(vec![temper_object, tools_object], tracker, &mut print)
                .map_err(|e| anyhow!(format_monty_exception(&e)))?
        };

        let mut pending_results: BTreeMap<u32, ExternalResult> = BTreeMap::new();

        loop {
            match progress {
                RunProgress::Complete(result) => {
                    let value = monty_object_to_json(&result);
                    return serde_json::to_string(&value)
                        .context("failed to serialize sandbox output as JSON string");
                }
                RunProgress::FunctionCall {
                    function_name,
                    args,
                    kwargs,
                    call_id,
                    method_call,
                    state,
                    ..
                } => {
                    if !method_call {
                        bail!(
                            "sandbox denied external function call '{}'. \
                             Only temper.<method> and tools.<method> are allowed",
                            function_name
                        );
                    }

                    let result = self.dispatch_method(&function_name, &args, &kwargs).await;
                    let ext_result = match result {
                        Ok(value) => ExternalResult::Return(json_to_monty_object(&value)),
                        Err(message) => ExternalResult::Error(MontyException::new(
                            ExcType::RuntimeError,
                            Some(message),
                        )),
                    };

                    pending_results.insert(call_id, ext_result);
                    // Fresh PrintWriter per call — Disabled has no state, and
                    // this avoids holding a non-Send type across await points.
                    let mut print = PrintWriter::Disabled;
                    progress = state
                        .run_pending(&mut print)
                        .map_err(|e| anyhow!(format_monty_exception(&e)))?;
                }
                RunProgress::ResolveFutures(state) => {
                    let mut ready: Vec<(u32, ExternalResult)> = Vec::new();
                    for call_id in state.pending_call_ids() {
                        if let Some(result) = pending_results.remove(call_id) {
                            ready.push((*call_id, result));
                        }
                    }

                    if ready.is_empty() {
                        bail!(
                            "sandbox is waiting on unresolved external calls: {:?}",
                            state.pending_call_ids()
                        );
                    }

                    let mut print = PrintWriter::Disabled;
                    progress = state
                        .resume(ready, &mut print)
                        .map_err(|e| anyhow!(format_monty_exception(&e)))?;
                }
                RunProgress::OsCall { function, .. } => {
                    bail!(
                        "sandbox blocked OS access ({function:?}). \
                         Use tools.bash(), tools.read(), tools.write(), or tools.ls() instead."
                    );
                }
            }
        }
    }

    /// Route a method call to the appropriate namespace.
    async fn dispatch_method(
        &self,
        function_name: &str,
        args: &[MontyObject],
        kwargs: &[(MontyObject, MontyObject)],
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
                self.dispatch_temper_method(function_name, args, kwargs)
                    .await
            }
            "Tools" => self.dispatch_tools_method(function_name, args).await,
            _ => Err(format!(
                "unknown namespace '{namespace}' for method '{function_name}'. \
                 Use temper.<method> or tools.<method>."
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::helpers::wrap_user_code;

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
