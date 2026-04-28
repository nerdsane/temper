use crate::types::WasmInvocationContext;

fn has_header(headers: &[(String, String)], name: &str) -> bool {
    headers
        .iter()
        .any(|(key, _)| key.eq_ignore_ascii_case(name))
}

pub(crate) fn add_workflow_observability_headers(
    mut builder: reqwest::RequestBuilder,
    headers: &[(String, String)],
    context: &WasmInvocationContext,
) -> reqwest::RequestBuilder {
    if !has_header(headers, "x-temper-workflow-root-entity-type")
        && let Some(value) = context
            .workflow_root_entity_type
            .as_deref()
            .filter(|value| !value.is_empty())
    {
        builder = builder.header("x-temper-workflow-root-entity-type", value);
    }
    if !has_header(headers, "x-temper-workflow-root-entity-id")
        && let Some(value) = context
            .workflow_root_entity_id
            .as_deref()
            .filter(|value| !value.is_empty())
    {
        builder = builder.header("x-temper-workflow-root-entity-id", value);
    }
    if !has_header(headers, "x-temper-workflow-run-id")
        && let Some(value) = context
            .workflow_run_id
            .as_deref()
            .filter(|value| !value.is_empty())
    {
        builder = builder.header("x-temper-workflow-run-id", value);
    }
    builder
}
