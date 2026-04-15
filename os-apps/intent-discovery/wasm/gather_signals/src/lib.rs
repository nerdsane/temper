use temper_wasm_sdk::prelude::*;

temper_module! {
    fn run(ctx: Context) -> Result<Value> {
        let fields = ctx.entity_state.get("fields").cloned().unwrap_or_else(|| json!({}));
        let base_url = temper_api_url(&ctx, &fields);

        let headers = internal_headers(&ctx.tenant);
        let unmet = get_json(&ctx, &format!("{base_url}/observe/evolution/unmet-intents"), &headers)
            .unwrap_or_else(|_| json!({"intents": []}));
        let intent_evidence = get_json(
            &ctx,
            &format!("{base_url}/observe/evolution/intent-evidence"),
            &headers,
        )
        .unwrap_or_else(|_| {
            json!({
                "intent_candidates": [],
                "workaround_patterns": [],
                "abandonment_patterns": [],
                "trajectory_samples": []
            })
        });
        let agents = get_json(&ctx, &format!("{base_url}/observe/agents"), &headers)
            .unwrap_or_else(|_| json!({"agents": []}));
        let suggestions = get_json(
            &ctx,
            &format!("{base_url}/api/tenants/{}/policies/suggestions", ctx.tenant),
            &headers,
        )
        .unwrap_or_else(|_| json!({"suggestions": []}));
        let specs = get_json(&ctx, &format!("{base_url}/observe/specs"), &headers)
            .unwrap_or_else(|_| json!({"specs": []}));
        let records = get_json(&ctx, &format!("{base_url}/observe/evolution/records"), &headers)
            .unwrap_or_else(|_| json!({"records": []}));
        let feature_requests = get_json(
            &ctx,
            &format!("{base_url}/observe/evolution/feature-requests"),
            &headers,
        )
        .unwrap_or_else(|_| json!({"feature_requests": []}));
        let issues = get_json(&ctx, &format!("{base_url}/tdata/Issues"), &headers)
            .unwrap_or_else(|_| json!({"value": []}));
        let comments = get_json(&ctx, &format!("{base_url}/tdata/Comments"), &headers)
            .unwrap_or_else(|_| json!({"value": []}));
        let plans = get_json(&ctx, &format!("{base_url}/tdata/Plans"), &headers)
            .unwrap_or_else(|_| json!({"value": []}));
        let projects = get_json(&ctx, &format!("{base_url}/tdata/Projects"), &headers)
            .unwrap_or_else(|_| json!({"value": []}));

        let reason = fields.get("reason").and_then(Value::as_str).unwrap_or("manual");
        let source = fields.get("source").and_then(Value::as_str).unwrap_or("manual");
        let trigger_context_json = fields
            .get("trigger_context_json")
            .and_then(Value::as_str)
            .unwrap_or("{}");
        let trigger_context = serde_json::from_str::<Value>(trigger_context_json).unwrap_or_else(|_| json!({}));

        let unmet_items = unmet
            .get("intents")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let intent_candidate_items = intent_evidence
            .get("intent_candidates")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let workaround_items = intent_evidence
            .get("workaround_patterns")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let abandonment_items = intent_evidence
            .get("abandonment_patterns")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let trajectory_items = intent_evidence
            .get("trajectory_samples")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let agent_items = agents
            .get("agents")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let suggestion_items = suggestions
            .get("suggestions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let spec_items = specs
            .get("specs")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let record_items = records
            .get("records")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let feature_items = feature_requests
            .get("feature_requests")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let issue_items = issues
            .get("value")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let comment_items = comments
            .get("value")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let plan_items = plans
            .get("value")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let project_items = projects
            .get("value")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let summary = json!({
            "tenant": ctx.tenant,
            "reason": reason,
            "source": source,
            "trigger_context": trigger_context,
            "signal_counts": {
                "unmet_intents": unmet_items.len(),
                "intent_candidates": intent_candidate_items.len(),
                "workaround_patterns": workaround_items.len(),
                "abandonment_patterns": abandonment_items.len(),
                "trajectory_samples": trajectory_items.len(),
                "agents": agent_items.len(),
                "policy_suggestions": suggestion_items.len(),
                "specs": spec_items.len(),
                "evolution_records": record_items.len(),
                "feature_requests": feature_items.len(),
                "issues": issue_items.len(),
                "comments": comment_items.len(),
                "plans": plan_items.len(),
                "projects": project_items.len()
            },
            "legacy_unmet_intents": unmet_items.into_iter().take(10).collect::<Vec<_>>(),
            "intent_evidence": {
                "intent_candidates": intent_candidate_items.into_iter().take(12).collect::<Vec<_>>(),
                "workaround_patterns": workaround_items.into_iter().take(8).collect::<Vec<_>>(),
                "abandonment_patterns": abandonment_items.into_iter().take(8).collect::<Vec<_>>(),
                "trajectory_samples": trajectory_items.into_iter().take(20).collect::<Vec<_>>()
            },
            "agents": agent_items.into_iter().take(10).collect::<Vec<_>>(),
            "policy_suggestions": suggestion_items.into_iter().take(10).collect::<Vec<_>>(),
            "specs": spec_items.into_iter().take(20).collect::<Vec<_>>(),
            "recent_records": record_items.into_iter().take(20).collect::<Vec<_>>(),
            "feature_requests": feature_items.into_iter().take(10).collect::<Vec<_>>(),
            "issues": issue_items.into_iter().take(20).collect::<Vec<_>>(),
            "comments": comment_items.into_iter().take(20).collect::<Vec<_>>(),
            "plans": plan_items.into_iter().take(10).collect::<Vec<_>>(),
            "projects": project_items.into_iter().take(10).collect::<Vec<_>>()
        });

        let signal_sources = json!([
            "GET /observe/evolution/unmet-intents",
            "GET /observe/evolution/intent-evidence",
            "GET /observe/agents",
            format!("GET /api/tenants/{}/policies/suggestions", ctx.tenant),
            "GET /observe/specs",
            "GET /observe/evolution/records",
            "GET /observe/evolution/feature-requests",
            "GET /tdata/Issues",
            "GET /tdata/Comments",
            "GET /tdata/Plans",
            "GET /tdata/Projects"
        ]);

        Ok(json!({
            "signal_summary_json": summary.to_string(),
            "signal_sources_json": signal_sources.to_string(),
            "signal_count": summary
                .get("signal_counts")
                .and_then(Value::as_object)
                .map(|counts| counts.values().filter_map(Value::as_u64).sum::<u64>())
                .unwrap_or(0)
        }))
    }
}

fn temper_api_url(ctx: &Context, fields: &Value) -> String {
    if let Some(value) = direct_config_base_url(ctx) {
        return value;
    }
    if let Some(value) = base_url_from_trigger_context(fields) {
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

fn internal_headers(tenant: &str) -> Vec<(String, String)> {
    vec![
        ("Content-Type".to_string(), "application/json".to_string()),
        ("Accept".to_string(), "application/json".to_string()),
        ("X-Tenant-Id".to_string(), tenant.to_string()),
        ("x-temper-principal-kind".to_string(), "admin".to_string()),
        ("x-temper-principal-id".to_string(), "intent-discovery".to_string()),
    ]
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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn resolves_base_url_from_trigger_context_port() {
        let fields = json!({
            "trigger_context_json": "{\"port\":4567}"
        });
        assert_eq!(
            base_url_from_trigger_context(&fields).as_deref(),
            Some("http://127.0.0.1:4567")
        );
    }
}
