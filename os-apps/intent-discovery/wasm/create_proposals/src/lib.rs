use temper_wasm_sdk::prelude::*;

temper_module! {
    fn run(ctx: Context) -> Result<Value> {
        let fields = ctx.entity_state.get("fields").cloned().unwrap_or_else(|| json!({}));
        let discovery_id = ctx
            .entity_state
            .get("entity_id")
            .and_then(Value::as_str)
            .unwrap_or("intent-discovery");
        let signal_summary_json = fields
            .get("signal_summary_json")
            .and_then(Value::as_str)
            .ok_or_else(|| "signal_summary_json missing from IntentDiscovery state".to_string())?;
        let analysis_json = fields
            .get("analysis_json")
            .and_then(Value::as_str)
            .ok_or_else(|| "analysis_json missing from IntentDiscovery state".to_string())?;
        let base_url = temper_api_url(&ctx, &fields, signal_summary_json, analysis_json);
        let headers = internal_headers(&ctx.tenant);

        let body = json!({
            "intent_discovery_id": discovery_id,
            "tenant": ctx.tenant,
            "reason": fields.get("reason").and_then(Value::as_str).unwrap_or("manual"),
            "source": fields.get("source").and_then(Value::as_str).unwrap_or("manual"),
            "signal_summary_json": signal_summary_json,
            "analysis_json": analysis_json,
        });
        let materialized = post_json(&ctx, &format!("{base_url}/api/evolution/materialize"), &headers, body)?;
        Ok(json!({
            "records_created_count": materialized.get("records_created_count").and_then(Value::as_u64).unwrap_or(0),
            "issues_created_count": materialized.get("issues_created_count").and_then(Value::as_u64).unwrap_or(0),
            "record_ids_json": materialized.get("record_ids").cloned().unwrap_or_else(|| json!([])).to_string(),
            "issue_ids_json": materialized.get("issue_ids").cloned().unwrap_or_else(|| json!([])).to_string(),
            "materialization_report_json": materialized.to_string(),
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

fn temper_api_url(
    ctx: &Context,
    fields: &Value,
    signal_summary_json: &str,
    analysis_json: &str,
) -> String {
    if let Some(value) = direct_config_base_url(ctx) {
        return value;
    }
    if let Some(value) = base_url_from_trigger_context(fields) {
        return value;
    }
    if let Some(value) = base_url_from_embedded_payload(signal_summary_json) {
        return value;
    }
    if let Some(value) = base_url_from_embedded_payload(analysis_json) {
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

fn base_url_from_embedded_payload(payload_json: &str) -> Option<String> {
    let payload = serde_json::from_str::<Value>(payload_json).ok()?;
    let trigger_context = payload.get("trigger_context")?;
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
    fn resolves_base_url_from_embedded_payload() {
        let payload = json!({
            "trigger_context": {
                "base_url": "http://127.0.0.1:4567"
            }
        });
        assert_eq!(
            base_url_from_embedded_payload(&payload.to_string()).as_deref(),
            Some("http://127.0.0.1:4567")
        );
    }
}
