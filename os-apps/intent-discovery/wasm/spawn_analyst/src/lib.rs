use temper_wasm_sdk::prelude::*;

const EVOLUTION_PROMPT: &str = include_str!("../../../../temper-agent/prompts/evolution_analyst.md");

temper_module! {
    fn run(ctx: Context) -> Result<Value> {
        let fields = ctx.entity_state.get("fields").cloned().unwrap_or_else(|| json!({}));
        let signal_summary_json = fields
            .get("signal_summary_json")
            .and_then(Value::as_str)
            .ok_or_else(|| "signal_summary_json missing from IntentDiscovery state".to_string())?;

        let base_url = temper_api_url(&ctx, &fields, signal_summary_json);
        let provider = ctx
            .config
            .get("provider")
            .cloned()
            .unwrap_or_else(|| "mock".to_string());
        let model = ctx
            .config
            .get("model")
            .cloned()
            .unwrap_or_else(|| "mock-evolution-analyst".to_string());
        let max_turns = ctx
            .config
            .get("max_turns")
            .cloned()
            .unwrap_or_else(|| "4".to_string());
        let agent_wait_timeout_ms = ctx
            .config
            .get("agent_wait_timeout_ms")
            .cloned()
            .unwrap_or_else(|| "120000".to_string());
        let agent_wait_poll_ms = ctx
            .config
            .get("agent_wait_poll_ms")
            .cloned()
            .unwrap_or_else(|| "250".to_string());
        let tools_enabled = ctx
            .config
            .get("tools_enabled")
            .cloned()
            .unwrap_or_default();
        let workdir = ctx
            .config
            .get("workdir")
            .cloned()
            .unwrap_or_else(|| "/workspace".to_string());
        let sandbox_url = ctx
            .config
            .get("sandbox_url")
            .cloned()
            .unwrap_or_else(|| "http://127.0.0.1:9999".to_string());

        let headers = internal_headers(&ctx.tenant);
        let discovery_id = ctx
            .entity_state
            .get("entity_id")
            .and_then(Value::as_str)
            .unwrap_or("intent-discovery");
        let agent_id = format!("intent-analyst-{}", sanitize_id(discovery_id));

        let create_url = format!("{base_url}/tdata/TemperAgents");
        let created = post_json(&ctx, &create_url, &headers, json!({ "id": agent_id }))?;
        let created_agent_id = extract_entity_id(&created).unwrap_or_else(|| agent_id.clone());

        let configure_url = format!(
            "{base_url}/tdata/TemperAgents('{created_agent_id}')/Temper.Agent.TemperAgent.Configure"
        );
        let _ = post_json(
            &ctx,
            &configure_url,
            &headers,
            json!({
                "system_prompt": EVOLUTION_PROMPT,
                "user_message": signal_summary_json,
                "model": model,
                "provider": provider,
                "max_turns": max_turns,
                "tools_enabled": tools_enabled,
                "workdir": workdir,
                "sandbox_url": sandbox_url,
            }),
        )?;

        let provision_url = format!(
            "{base_url}/tdata/TemperAgents('{created_agent_id}')/Temper.Agent.TemperAgent.Provision?await_integration=true"
        );
        let _provisioned = post_json(&ctx, &provision_url, &headers, json!({}))?;
        let completed = wait_for_terminal_agent_state(
            &ctx,
            &base_url,
            &headers,
            &created_agent_id,
            &agent_wait_timeout_ms,
            &agent_wait_poll_ms,
        )?;
        let status = entity_status(&completed);
        if status != "Completed" {
            return Err(format!("TemperAgent did not complete successfully: {status}"));
        }

        let analysis_json = completed
            .get("fields")
            .and_then(|f| f.get("result"))
            .and_then(Value::as_str)
            .or_else(|| completed.get("fields").and_then(|f| f.get("Result")).and_then(Value::as_str))
            .ok_or_else(|| "TemperAgent completed without a result payload".to_string())?;
        let parsed_analysis = serde_json::from_str::<Value>(analysis_json)
            .map_err(|e| format!("TemperAgent returned invalid analysis JSON: {e}"))?;
        let finding_count = parsed_analysis
            .get("findings")
            .and_then(Value::as_array)
            .map(|items| items.len() as u64)
            .unwrap_or(0);

        Ok(json!({
            "analyst_agent_id": created_agent_id,
            "analysis_json": analysis_json,
            "finding_count": finding_count,
        }))
    }
}

fn internal_headers(tenant: &str) -> Vec<(String, String)> {
    vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        ("Accept".to_string(), "application/json".to_string()),
        ("X-Tenant-Id".to_string(), tenant.to_string()),
        ("x-temper-principal-kind".to_string(), "admin".to_string()),
        ("x-temper-principal-id".to_string(), "intent-discovery".to_string()),
    ]
}

fn post_json(
    ctx: &Context,
    url: &str,
    headers: &[(String, String)],
    body: Value,
) -> Result<Value, String> {
    let resp = ctx.http_call("POST", url, headers, &body.to_string())?;
    if !(200..300).contains(&resp.status) {
        return Err(format!("POST {url} failed: HTTP {} body={}", resp.status, resp.body));
    }
    if resp.body.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str::<Value>(&resp.body)
        .map_err(|e| format!("failed to parse JSON from {url}: {e}"))
}

fn get_json(ctx: &Context, url: &str, headers: &[(String, String)]) -> Result<Value, String> {
    let resp = ctx.http_call("GET", url, headers, "")?;
    if !(200..300).contains(&resp.status) {
        return Err(format!("GET {url} failed: HTTP {} body={}", resp.status, resp.body));
    }
    if resp.body.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str::<Value>(&resp.body)
        .map_err(|e| format!("failed to parse JSON from {url}: {e}"))
}

fn entity_status(value: &Value) -> &str {
    value
        .get("status")
        .and_then(Value::as_str)
        .or_else(|| {
            value
                .get("fields")
                .and_then(|f| f.get("Status"))
                .and_then(Value::as_str)
        })
        .unwrap_or("Unknown")
}

fn wait_for_terminal_agent_state(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    agent_id: &str,
    timeout_ms: &str,
    poll_ms: &str,
) -> Result<Value, String> {
    let wait_url = format!(
        "{base_url}/observe/entities/TemperAgent/{agent_id}/wait?statuses=Completed,Failed,Cancelled&timeout_ms={timeout_ms}&poll_ms={poll_ms}"
    );
    let entity = get_json(ctx, &wait_url, headers)?;
    let status = entity_status(&entity).to_string();
    if matches!(status.as_str(), "Completed" | "Failed" | "Cancelled") {
        return Ok(entity);
    }
    let timed_out = entity
        .get("timed_out")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if timed_out {
        return Err(format!(
            "TemperAgent did not reach a terminal state within {timeout_ms}ms; last status: {status}"
        ));
    }
    Err(format!(
        "TemperAgent did not reach a terminal state after waiting; last status: {status}"
    ))
}

fn extract_entity_id(value: &Value) -> Option<String> {
    value
        .get("entity_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            value
                .get("fields")
                .and_then(|f| f.get("Id"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
}

fn sanitize_id(raw: &str) -> String {
    let mut out = String::new();
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else {
            out.push('-');
        }
    }
    out.chars().take(64).collect()
}

fn temper_api_url(ctx: &Context, fields: &Value, signal_summary_json: &str) -> String {
    if let Some(value) = direct_config_base_url(ctx) {
        return value;
    }
    if let Some(value) = base_url_from_trigger_context(fields) {
        return value;
    }
    if let Some(value) = base_url_from_signal_summary(signal_summary_json) {
        return value;
    }
    "http://127.0.0.1:3000".to_string()
}

fn direct_config_base_url(ctx: &Context) -> Option<String> {
    ctx.config
        .get("temper_api_url")
        .map(String::as_str)
        .filter(|value| !value.trim().is_empty() && !value.contains("{secret:"))
        .map(str::to_string)
}

fn base_url_from_trigger_context(fields: &Value) -> Option<String> {
    let trigger_context = fields
        .get("trigger_context_json")
        .and_then(Value::as_str)
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .unwrap_or_else(|| json!({}));
    explicit_base_url(&trigger_context)
        .or_else(|| port_base_url(&trigger_context))
        .or_else(|| host_port_base_url(&trigger_context))
}

fn base_url_from_signal_summary(signal_summary_json: &str) -> Option<String> {
    let summary = serde_json::from_str::<Value>(signal_summary_json).ok()?;
    let trigger_context = summary.get("trigger_context")?;
    explicit_base_url(trigger_context)
        .or_else(|| port_base_url(trigger_context))
        .or_else(|| host_port_base_url(trigger_context))
}

fn explicit_base_url(value: &Value) -> Option<String> {
    value
        .get("base_url")
        .or_else(|| value.get("temper_api_url"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
        .map(str::to_string)
}

fn port_base_url(value: &Value) -> Option<String> {
    value
        .get("port")
        .and_then(Value::as_u64)
        .map(|port| format!("http://127.0.0.1:{port}"))
}

fn host_port_base_url(value: &Value) -> Option<String> {
    let host = value
        .get("host")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|raw| !raw.is_empty())?;
    let port = value.get("port").and_then(Value::as_u64)?;
    Some(format!("http://{host}:{port}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_base_url_from_signal_summary_trigger_context() {
        let signal_summary = json!({
            "trigger_context": {
                "port": 4567
            }
        });
        assert_eq!(
            base_url_from_signal_summary(&signal_summary.to_string()).as_deref(),
            Some("http://127.0.0.1:4567")
        );
    }

    #[test]
    fn resolves_base_url_from_trigger_context_base_url() {
        let fields = json!({
            "trigger_context_json": "{\"base_url\":\"http://127.0.0.1:4567\"}"
        });
        assert_eq!(
            base_url_from_trigger_context(&fields).as_deref(),
            Some("http://127.0.0.1:4567")
        );
    }
}
