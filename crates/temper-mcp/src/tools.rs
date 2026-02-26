//! Temper tool dispatch for MCP execute sandbox calls.

use monty::MontyObject;
use reqwest::Method;
use serde_json::Value;

use super::runtime::RuntimeContext;
use super::sandbox::{
    escape_odata_key, expect_json_object_arg, expect_string_arg, format_http_error,
    format_authz_denied,
};

impl RuntimeContext {
    pub(super) async fn dispatch_temper_method(
        &self,
        method: &str,
        args: &[MontyObject],
        kwargs: &[(MontyObject, MontyObject)],
    ) -> std::result::Result<Value, String> {
        if !kwargs.is_empty() {
            return Err(format!(
                "temper.{method} does not support keyword arguments in this MCP server"
            ));
        }

        // Dataclass method calls include self as the first arg.
        let args = if args.is_empty() { args } else { &args[1..] };

        match method {
            "list" => {
                let tenant = expect_string_arg(args, 0, "tenant", method)?;
                let entity = expect_string_arg(args, 1, "entity_type", method)?;
                let set = self.resolve_entity_set(&tenant, &entity);

                let body = self
                    .temper_request(&tenant, Method::GET, format!("/tdata/{set}"), None)
                    .await?;
                Ok(body.get("value").cloned().unwrap_or(body))
            }
            "get" => {
                let tenant = expect_string_arg(args, 0, "tenant", method)?;
                let entity = expect_string_arg(args, 1, "entity_type", method)?;
                let entity_id = expect_string_arg(args, 2, "entity_id", method)?;
                let set = self.resolve_entity_set(&tenant, &entity);
                let key = escape_odata_key(&entity_id);

                self.temper_request(&tenant, Method::GET, format!("/tdata/{set}('{key}')"), None)
                    .await
            }
            "create" => {
                let tenant = expect_string_arg(args, 0, "tenant", method)?;
                let entity = expect_string_arg(args, 1, "entity_type", method)?;
                let fields = expect_json_object_arg(args, 2, "fields", method)?;
                let set = self.resolve_entity_set(&tenant, &entity);

                self.temper_request(
                    &tenant,
                    Method::POST,
                    format!("/tdata/{set}"),
                    Some(Value::Object(fields)),
                )
                .await
            }
            "action" => {
                let tenant = expect_string_arg(args, 0, "tenant", method)?;
                let entity = expect_string_arg(args, 1, "entity_type", method)?;
                let entity_id = expect_string_arg(args, 2, "entity_id", method)?;
                let action_name = expect_string_arg(args, 3, "action_name", method)?;
                let body = expect_json_object_arg(args, 4, "body", method)?;
                let set = self.resolve_entity_set(&tenant, &entity);
                let key = escape_odata_key(&entity_id);

                self.temper_request(
                    &tenant,
                    Method::POST,
                    format!("/tdata/{set}('{key}')/Temper.{action_name}"),
                    Some(Value::Object(body)),
                )
                .await
            }
            "patch" => {
                let tenant = expect_string_arg(args, 0, "tenant", method)?;
                let entity = expect_string_arg(args, 1, "entity_type", method)?;
                let entity_id = expect_string_arg(args, 2, "entity_id", method)?;
                let fields = expect_json_object_arg(args, 3, "fields", method)?;
                let set = self.resolve_entity_set(&tenant, &entity);
                let key = escape_odata_key(&entity_id);

                self.temper_request(
                    &tenant,
                    Method::PATCH,
                    format!("/tdata/{set}('{key}')"),
                    Some(Value::Object(fields)),
                )
                .await
            }
            // --- Developer methods ---
            "show_spec" => {
                let tenant = expect_string_arg(args, 0, "tenant", method)?;
                let entity_type = expect_string_arg(args, 1, "entity_type", method)?;
                self.spec
                    .get(&tenant)
                    .and_then(|v| v.get("entities"))
                    .and_then(|v| v.get(&entity_type))
                    .cloned()
                    .ok_or_else(|| format!("No spec found for {tenant}/{entity_type}"))
            }
            "submit_specs" => {
                let tenant = expect_string_arg(args, 0, "tenant", method)?;
                let specs = expect_json_object_arg(args, 1, "specs", method)?;
                let payload = serde_json::json!({ "tenant": tenant, "specs": specs });
                self.temper_request(
                    &tenant,
                    Method::POST,
                    "/api/specs/load-inline".to_string(),
                    Some(payload),
                )
                .await
            }
            "set_policy" => {
                let tenant = expect_string_arg(args, 0, "tenant", method)?;
                let policy_text = expect_string_arg(args, 1, "policy_text", method)?;
                self.temper_request_text(
                    &tenant,
                    Method::PUT,
                    format!("/api/tenants/{tenant}/policies"),
                    Some(policy_text),
                )
                .await
            }
            "get_policies" => {
                let tenant = expect_string_arg(args, 0, "tenant", method)?;
                self.temper_request(
                    &tenant,
                    Method::GET,
                    format!("/api/tenants/{tenant}/policies"),
                    None,
                )
                .await
            }
            // --- Governance methods ---
            "get_decisions" => {
                let tenant = expect_string_arg(args, 0, "tenant", method)?;
                let status = args.get(1).and_then(|a| String::try_from(a).ok());
                let path = match status {
                    Some(s) => format!("/api/tenants/{tenant}/decisions?status={s}"),
                    None => format!("/api/tenants/{tenant}/decisions"),
                };
                self.temper_request(&tenant, Method::GET, path, None).await
            }
            "approve_decision" => {
                let tenant = expect_string_arg(args, 0, "tenant", method)?;
                let decision_id = expect_string_arg(args, 1, "decision_id", method)?;
                let scope = expect_string_arg(args, 2, "scope", method)?;
                let payload = serde_json::json!({ "scope": scope, "decided_by": "mcp-agent" });
                self.temper_request(
                    &tenant,
                    Method::POST,
                    format!("/api/tenants/{tenant}/decisions/{decision_id}/approve"),
                    Some(payload),
                )
                .await
            }
            "deny_decision" => {
                let tenant = expect_string_arg(args, 0, "tenant", method)?;
                let decision_id = expect_string_arg(args, 1, "decision_id", method)?;
                self.temper_request(
                    &tenant,
                    Method::POST,
                    format!("/api/tenants/{tenant}/decisions/{decision_id}/deny"),
                    None,
                )
                .await
            }
            "poll_decision" => {
                let tenant = expect_string_arg(args, 0, "tenant", method)?;
                let decision_id = expect_string_arg(args, 1, "decision_id", method)?;
                let start = std::time::Instant::now(); // determinism-ok: wall-clock for MCP timeout only
                loop {
                    let result = self
                        .temper_request(
                            &tenant,
                            Method::GET,
                            format!("/api/tenants/{tenant}/decisions"),
                            None,
                        )
                        .await?;
                    if let Some(decisions) = result.as_array() {
                        for d in decisions {
                            if d.get("id").and_then(Value::as_str) == Some(&decision_id) {
                                let status =
                                    d.get("status").and_then(Value::as_str).unwrap_or("");
                                if status != "Pending" {
                                    return Ok(d.clone());
                                }
                            }
                        }
                    }
                    if start.elapsed() > std::time::Duration::from_secs(30) {
                        return Err(format!(
                            "poll_decision timed out after 30s: decision {decision_id} still pending"
                        ));
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
                }
            }
            _ => Err(format!(
                "unknown temper method '{method}'. Available: list, get, create, action, patch, \
                 show_spec, submit_specs, set_policy, get_policies, \
                 get_decisions, approve_decision, deny_decision, poll_decision"
            )),
        }
    }

    fn resolve_entity_set(&self, tenant: &str, entity_or_set: &str) -> String {
        if let Some(metadata) = self.app_metadata.get(tenant) {
            if metadata.entity_set_to_type.contains_key(entity_or_set) {
                return entity_or_set.to_string();
            }
            if let Some(set) = metadata.entity_type_to_set.get(entity_or_set) {
                return set.clone();
            }
            let plural_guess = format!("{entity_or_set}s");
            if metadata.entity_set_to_type.contains_key(&plural_guess) {
                return plural_guess;
            }
        }
        entity_or_set.to_string()
    }

    async fn temper_request(
        &self,
        tenant: &str,
        method: Method,
        path: String,
        body: Option<Value>,
    ) -> std::result::Result<Value, String> {
        let url = format!("http://127.0.0.1:{}{path}", self.temper_port);
        let mut request = self
            .http
            .request(method, &url)
            .header("X-Tenant-Id", tenant)
            .header("Accept", "application/json");

        if let Some(ref pid) = self.principal_id {
            request = request
                .header("X-Temper-Principal-Kind", "agent")
                .header("X-Temper-Principal-Id", pid.as_str());
        }

        if let Some(ref payload) = body {
            request = request.json(payload);
        }

        let response = request
            .send()
            .await
            .map_err(|e| format!("failed to call Temper at {url}: {e}"))?;

        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| format!("failed to read Temper response body: {e}"))?;

        if status.is_success() {
            if text.trim().is_empty() {
                return Ok(Value::Null);
            }
            return serde_json::from_str(&text).or_else(|_| Ok(Value::String(text)));
        }

        if status == reqwest::StatusCode::FORBIDDEN {
            if let Some(rich) = format_authz_denied(&text) {
                return Err(rich);
            }
        }

        Err(format_http_error(status, &text))
    }

    /// Send a request with a plain-text body (e.g. Cedar policy text).
    async fn temper_request_text(
        &self,
        tenant: &str,
        method: Method,
        path: String,
        body: Option<String>,
    ) -> std::result::Result<Value, String> {
        let url = format!("http://127.0.0.1:{}{path}", self.temper_port);
        let mut request = self
            .http
            .request(method, &url)
            .header("X-Tenant-Id", tenant);

        if let Some(ref pid) = self.principal_id {
            request = request
                .header("X-Temper-Principal-Kind", "agent")
                .header("X-Temper-Principal-Id", pid.as_str());
        }

        if let Some(ref text) = body {
            request = request
                .header("Content-Type", "text/plain")
                .body(text.clone());
        }

        let response = request
            .send()
            .await
            .map_err(|e| format!("failed to call Temper at {url}: {e}"))?;

        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| format!("failed to read Temper response body: {e}"))?;

        if status.is_success() {
            if text.trim().is_empty() {
                return Ok(Value::Null);
            }
            return serde_json::from_str(&text).or_else(|_| Ok(Value::String(text)));
        }

        Err(format_http_error(status, &text))
    }
}
