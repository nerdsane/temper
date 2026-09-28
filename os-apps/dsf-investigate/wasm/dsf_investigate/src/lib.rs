//! Deep Sci-Fi Datadog investigation hands.
//!
//! Validates credentials and searches logs over the Datadog HTTP API.
//! Secret values are never logged or returned.

use temper_wasm_sdk::prelude::*;

temper_module! {
    fn run(ctx: Context) -> Result<Value> {
        match ctx.trigger_action.as_str() {
            "StartGather" | "Resume" => gather(&ctx),
            other => Err(format!("dsf_investigate: unsupported trigger action {other}")),
        }
    }
}

fn gather(ctx: &Context) -> Result<Value, String> {
    let creds = datadog_creds(ctx)?;
    let fields = entity_fields(ctx);
    let service = field_str(&fields, "Service");
    let query = field_str(&fields, "Query");
    let time_range = field_str(&fields, "TimeRange");
    let api_base = datadog_api_base(&creds.site);
    ctx.log(
        "info",
        &format!("dsf_investigate: validating Datadog API at {api_base}"),
    );

    let headers = datadog_headers(&creds);
    let validate_url = format!("{api_base}/api/v1/validate");
    let validate = ctx.http_call("GET", &validate_url, &headers, "")?;
    if !(200..300).contains(&validate.status) {
        return Err(format!(
            "dsf_investigate: Datadog validate failed HTTP {} (body not printed)",
            validate.status
        ));
    }

    let filter_query = logs_query(service, query);
    let from = logs_from(time_range);
    let search_url = format!("{api_base}/api/v2/logs/events/search");
    let body = json!({
        "filter": {
            "query": filter_query,
            "from": from,
            "to": "now"
        },
        "page": { "limit": 20 }
    })
    .to_string();
    ctx.log(
        "info",
        "dsf_investigate: searching Datadog logs (query not logged if it could contain secrets)",
    );
    let search = ctx.http_call("POST", &search_url, &headers, &body)?;
    if !(200..300).contains(&search.status) {
        return Err(format!(
            "dsf_investigate: Datadog logs search failed HTTP {} (body not printed)",
            search.status
        ));
    }
    let count = finding_count(&search.body);
    Ok(json!({ "FindingCount": count.to_string() }))
}

struct DatadogCreds {
    site: String,
    access_token: Option<String>,
    api_key: Option<String>,
    app_key: Option<String>,
}

fn datadog_creds(ctx: &Context) -> Result<DatadogCreds, String> {
    let fields = entity_fields(ctx);
    let site = lookup(ctx, &fields, "dd_site")
        .or_else(|| lookup(ctx, &fields, "DD_SITE"))
        .unwrap_or("datadoghq.com")
        .to_string();
    let access_token = lookup(ctx, &fields, "dd_access_token")
        .or_else(|| lookup(ctx, &fields, "DD_ACCESS_TOKEN"))
        .map(str::to_string);
    let api_key = lookup(ctx, &fields, "dd_api_key")
        .or_else(|| lookup(ctx, &fields, "DD_API_KEY"))
        .map(str::to_string);
    let app_key = lookup(ctx, &fields, "dd_app_key")
        .or_else(|| lookup(ctx, &fields, "DD_APP_KEY"))
        .map(str::to_string);

    let has_token = access_token.as_ref().is_some_and(|value| !value.is_empty());
    let has_keys = api_key.as_ref().is_some_and(|value| !value.is_empty())
        && app_key.as_ref().is_some_and(|value| !value.is_empty());
    if !has_token && !has_keys {
        return Err(
            "dsf_investigate: Datadog credentials are not set. Stock TensorLake sandbox dsf \
             with pup and set DD_ACCESS_TOKEN, or both DD_API_KEY and DD_APP_KEY, plus DD_SITE. \
             Values are never printed. Run os-apps/dsf-deploy/scripts/stock_dsf_sandbox.sh."
                .to_string(),
        );
    }
    Ok(DatadogCreds {
        site,
        access_token,
        api_key,
        app_key,
    })
}

fn datadog_api_base(site: &str) -> String {
    let site = site
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    if site.is_empty() {
        return "https://api.datadoghq.com".to_string();
    }
    if site.starts_with("api.") {
        return format!("https://{site}");
    }
    format!("https://api.{site}")
}

fn datadog_headers(creds: &DatadogCreds) -> Vec<(String, String)> {
    let mut headers = vec![
        ("content-type".to_string(), "application/json".to_string()),
        ("accept".to_string(), "application/json".to_string()),
    ];
    if let Some(token) = creds
        .access_token
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        headers.push(("authorization".to_string(), format!("Bearer {token}")));
        return headers;
    }
    if let (Some(api_key), Some(app_key)) = (
        creds.api_key.as_deref().filter(|value| !value.is_empty()),
        creds.app_key.as_deref().filter(|value| !value.is_empty()),
    ) {
        headers.push(("dd-api-key".to_string(), api_key.to_string()));
        headers.push(("dd-application-key".to_string(), app_key.to_string()));
    }
    headers
}

fn logs_query(service: &str, query: &str) -> String {
    match (service.is_empty(), query.is_empty()) {
        (false, false) => format!("service:{service} {query}"),
        (false, true) => format!("service:{service}"),
        (true, false) => query.to_string(),
        (true, true) => "*".to_string(),
    }
}

fn logs_from(time_range: &str) -> String {
    let trimmed = time_range.trim();
    if trimmed.is_empty() {
        return "now-1h".to_string();
    }
    if trimmed.starts_with("now") || trimmed.contains('-') && trimmed.contains(':') {
        return trimmed.to_string();
    }
    format!("now-{trimmed}")
}

fn finding_count(body: &str) -> usize {
    let Ok(parsed) = serde_json::from_str::<Value>(body) else {
        return 0;
    };
    parsed
        .get("data")
        .and_then(Value::as_array)
        .map(Vec::len)
        .or_else(|| {
            parsed
                .get("meta")
                .and_then(|meta| meta.get("page"))
                .and_then(|page| page.get("after").or_else(|| page.get("total")))
                .and_then(Value::as_u64)
                .map(|n| n as usize)
        })
        .unwrap_or(0)
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

fn usable(raw: Option<&str>) -> Option<&str> {
    let value = raw?.trim();
    if value.is_empty() || value.contains("{secret:") {
        None
    } else {
        Some(value)
    }
}

fn lookup<'a>(ctx: &'a Context, fields: &'a Value, key: &str) -> Option<&'a str> {
    usable(fields.get(key).and_then(Value::as_str))
        .or_else(|| usable(ctx.config.get(key).map(String::as_str)))
        .or_else(|| usable(ctx.trigger_params.get(key).and_then(Value::as_str)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_base_prefixes_site() {
        assert_eq!(
            datadog_api_base("datadoghq.com"),
            "https://api.datadoghq.com"
        );
        assert_eq!(
            datadog_api_base("us3.datadoghq.com"),
            "https://api.us3.datadoghq.com"
        );
        assert_eq!(
            datadog_api_base("https://api.datadoghq.eu"),
            "https://api.datadoghq.eu"
        );
        assert_eq!(datadog_api_base(""), "https://api.datadoghq.com");
    }

    #[test]
    fn logs_query_combines_service_and_query() {
        assert_eq!(
            logs_query("api", "status:error"),
            "service:api status:error"
        );
        assert_eq!(logs_query("api", ""), "service:api");
        assert_eq!(logs_query("", "status:error"), "status:error");
        assert_eq!(logs_query("", ""), "*");
    }

    #[test]
    fn logs_from_accepts_relative_and_absolute() {
        assert_eq!(logs_from(""), "now-1h");
        assert_eq!(logs_from("15m"), "now-15m");
        assert_eq!(logs_from("now-2h"), "now-2h");
    }

    #[test]
    fn finding_count_reads_data_array() {
        assert_eq!(finding_count(r#"{"data":[{},{}]}"#), 2);
        assert_eq!(finding_count("not-json"), 0);
    }
}
