//! Deep Sci-Fi deploy hands.
//!
//! Probe, deploy, and verify over HTTP. Missing URLs fail closed with
//! operator instructions. Secret values are never logged.

use temper_wasm_sdk::prelude::*;

temper_module! {
    fn run(ctx: Context) -> Result<Value> {
        match ctx.trigger_action.as_str() {
            "ProbeHealth" | "ResumeOnComputer" => probe_health(&ctx),
            "StartDeploy" => start_deploy(&ctx),
            "StartVerify" => start_verify(&ctx),
            other => Err(format!("dsf_deploy: unsupported trigger action {other}")),
        }
    }
}

fn probe_health(ctx: &Context) -> Result<Value, String> {
    let url = health_url(ctx).ok_or_else(missing_health_url)?;
    ctx.log("info", &format!("dsf_deploy: probing {url}"));
    let resp = ctx.http_call("GET", &url, &[], "")?;
    if !(200..300).contains(&resp.status) {
        return Err(format!(
            "dsf_deploy: health probe failed HTTP {} (body truncated, secrets not printed)",
            resp.status
        ));
    }
    Ok(json!({}))
}

fn start_deploy(ctx: &Context) -> Result<Value, String> {
    let url = deploy_url(ctx).ok_or_else(missing_deploy_url)?;
    let fields = entity_fields(ctx);
    let service = field_str(&fields, "Service");
    let environment = field_str(&fields, "Environment");
    let computer = field_str(&fields, "ComputerName");
    ctx.log("info", &format!("dsf_deploy: deploying via {url}"));
    let body = json!({
        "service": service,
        "environment": environment,
        "computer_name": computer,
    })
    .to_string();
    let headers = vec![("content-type".to_string(), "application/json".to_string())];
    let resp = ctx.http_call("POST", &url, &headers, &body)?;
    if !(200..300).contains(&resp.status) {
        return Err(format!(
            "dsf_deploy: deploy failed HTTP {} (body truncated, secrets not printed)",
            resp.status
        ));
    }
    let deploy_id = deploy_id_from_body(&resp.body).unwrap_or_else(|| {
        format!(
            "dsf-{}-{}-{}",
            empty_fallback(service, "service"),
            empty_fallback(environment, "env"),
            resp.status
        )
    });
    Ok(json!({ "DeployId": deploy_id }))
}

fn start_verify(ctx: &Context) -> Result<Value, String> {
    let url = verify_url(ctx).ok_or_else(missing_health_url)?;
    ctx.log("info", &format!("dsf_deploy: verifying {url}"));
    let resp = ctx.http_call("GET", &url, &[], "")?;
    if !(200..300).contains(&resp.status) {
        return Err(format!(
            "dsf_deploy: verify failed HTTP {} (body truncated, secrets not printed)",
            resp.status
        ));
    }
    Ok(json!({}))
}

fn entity_fields(ctx: &Context) -> Value {
    ctx.entity_state
        .get("fields")
        .cloned()
        .unwrap_or_else(|| json!({}))
}

fn field_str<'a>(fields: &'a Value, key: &str) -> &'a str {
    fields.get(key).and_then(Value::as_str).unwrap_or("")
}

fn empty_fallback<'a>(value: &'a str, fallback: &'a str) -> &'a str {
    if value.is_empty() { fallback } else { value }
}

fn usable(raw: Option<&str>) -> Option<&str> {
    let value = raw?.trim();
    if value.is_empty() || value.contains("{secret:") {
        None
    } else {
        Some(value)
    }
}

fn first_usable<'a>(candidates: impl IntoIterator<Item = Option<&'a str>>) -> Option<&'a str> {
    candidates.into_iter().find_map(usable)
}

fn lookup<'a>(ctx: &'a Context, fields: &'a Value, key: &str) -> Option<&'a str> {
    first_usable([
        fields.get(key).and_then(Value::as_str),
        ctx.config.get(key).map(String::as_str),
        ctx.trigger_params.get(key).and_then(Value::as_str),
    ])
}

fn sandbox_base<'a>(ctx: &'a Context, fields: &'a Value) -> Option<&'a str> {
    lookup(ctx, fields, "sandbox_url")
}

fn join_sandbox(base: &str, path: &str) -> String {
    format!("{}{path}", base.trim_end_matches('/'))
}

fn health_url(ctx: &Context) -> Option<String> {
    let fields = entity_fields(ctx);
    if let Some(url) = lookup(ctx, &fields, "health_url") {
        return Some(url.to_string());
    }
    sandbox_base(ctx, &fields).map(|base| join_sandbox(base, "/health"))
}

fn deploy_url(ctx: &Context) -> Option<String> {
    let fields = entity_fields(ctx);
    if let Some(url) = lookup(ctx, &fields, "deploy_url") {
        return Some(url.to_string());
    }
    sandbox_base(ctx, &fields).map(|base| join_sandbox(base, "/deploy"))
}

fn verify_url(ctx: &Context) -> Option<String> {
    let fields = entity_fields(ctx);
    if let Some(url) = lookup(ctx, &fields, "verify_url") {
        return Some(url.to_string());
    }
    health_url(ctx)
}

fn deploy_id_from_body(body: &str) -> Option<String> {
    let parsed: Value = serde_json::from_str(body).ok()?;
    parsed
        .get("DeployId")
        .or_else(|| parsed.get("deploy_id"))
        .or_else(|| parsed.get("id"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn missing_health_url() -> String {
    "dsf_deploy: no health_url (or sandbox_url) configured. Set health_url / \
     sandbox_url on the trigger or stock TensorLake sandbox dsf and point \
     TEMPER_SANDBOX_URL at it. This module does not invent a Railway API."
        .to_string()
}

fn missing_deploy_url() -> String {
    "dsf_deploy: no deploy_url (or sandbox_url) configured. Set deploy_url / \
     sandbox_url on the trigger or stock TensorLake sandbox dsf and point \
     TEMPER_SANDBOX_URL at it. This module does not invent a Railway API."
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usable_skips_empty_and_secret_templates() {
        assert_eq!(usable(None), None);
        assert_eq!(usable(Some("")), None);
        assert_eq!(usable(Some("{secret:health_url}")), None);
        assert_eq!(
            usable(Some("https://svc/health")),
            Some("https://svc/health")
        );
    }

    #[test]
    fn deploy_id_reads_known_keys() {
        assert_eq!(
            deploy_id_from_body(r#"{"DeployId":"dep-1"}"#).as_deref(),
            Some("dep-1")
        );
        assert_eq!(
            deploy_id_from_body(r#"{"deploy_id":"dep-2"}"#).as_deref(),
            Some("dep-2")
        );
        assert_eq!(
            deploy_id_from_body(r#"{"id":"dep-3"}"#).as_deref(),
            Some("dep-3")
        );
        assert_eq!(deploy_id_from_body("not-json"), None);
    }

    #[test]
    fn join_sandbox_avoids_double_slash() {
        assert_eq!(
            join_sandbox("https://dsf.example/", "/health"),
            "https://dsf.example/health"
        );
    }
}
