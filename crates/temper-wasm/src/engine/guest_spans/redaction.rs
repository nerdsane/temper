//! ADR-0166: what a guest may name in its own span attributes.
//!
//! A guest module chooses these keys, so the `gen_ai.*` namespace — the one LLM
//! Observability and the GenAI dashboards read — is attacker-chosen text. For a
//! tenant that has not opted into content export, only recognised metadata keys
//! survive inside that namespace, with bounded values.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::host_trait::span_hints::{
    clamp_llm_metadata_json, is_llm_namespace_key, llm_namespace_attr_allowed,
};

/// The single filter every guest-supplied span attribute passes through, on both
/// the OTel path and the manual-export path.
pub(super) fn allowed_attributes(
    attributes: &BTreeMap<String, Value>,
    export_llm_content: bool,
) -> BTreeMap<String, Value> {
    attributes
        .iter()
        .filter(|(key, _)| guest_span_attribute_allowed(key))
        .filter_map(|(key, value)| {
            redacted_guest_attribute_value(key, value, export_llm_content)
                .map(|value| (key.clone(), value))
        })
        .collect()
}

/// Apply ADR-0166 to one guest-supplied span attribute. Returns `None` when the
/// attribute must not be recorded at all.
///
/// A guest module names its own span attributes, so the `gen_ai.*` namespace —
/// the one LLM Observability and the GenAI dashboards read — is attacker-chosen
/// text. For a tenant that has not opted into content export, only recognised
/// metadata keys survive inside that namespace, and their values are bounded so
/// a prompt cannot ride inside `gen_ai.request.model`. Attributes outside the
/// namespace are the module's own application telemetry and pass through: they
/// carry no agreed meaning to redact against, and dropping them would remove
/// working guest observability. That boundary is stated in ADR-0166.
fn redacted_guest_attribute_value(
    key: &str,
    value: &Value,
    export_llm_content: bool,
) -> Option<Value> {
    if export_llm_content {
        return Some(value.clone());
    }
    if !llm_namespace_attr_allowed(key) {
        return None;
    }
    if !is_llm_namespace_key(key) {
        return Some(value.clone());
    }
    clamp_llm_metadata_json(value)
}

/// Keys a guest may not set, because the host owns them: OTel span identity and
/// the internal `_otel.*` correlation fields.
pub(super) fn guest_span_attribute_allowed(key: &str) -> bool {
    let key = key.trim();
    if key.is_empty() {
        return false;
    }
    !matches!(
        key,
        "otel.name"
            | "otel.kind"
            | "trace_id"
            | "span_id"
            | "parent_span_id"
            | "dd.trace_id"
            | "dd.span_id"
            | "otel.trace_id"
            | "otel.span_id"
            | "_otel.parent_trace_id"
            | "_otel.parent_span_id"
    ) && !key.starts_with("_otel.")
}
