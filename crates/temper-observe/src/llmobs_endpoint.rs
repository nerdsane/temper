pub(super) fn llmobs_endpoint(site: &str, endpoint_override: Option<String>) -> String {
    endpoint_override.unwrap_or_else(|| {
        let site = site.trim().trim_end_matches('/');
        format!("https://api.{site}/api/intake/llm-obs/v1/trace/spans")
    })
}
