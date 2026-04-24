//! Shared helper for merging static and dynamic reaction params.
//!
//! Used by both the async [`super::dispatcher::ReactionDispatcher`] and the
//! deterministic [`super::sim_dispatcher::SimReactionSystem`] so production
//! and simulation apply identical merge semantics.

use super::types::ReactionTarget;

/// Build the effective params for a reaction dispatch.
///
/// Starts from the static `target.params` and overlays values read out of the
/// source entity's `source_fields` according to `target.params_from`.
///
/// Semantics:
/// * Static `params` is expected to be a JSON object (or null/absent, treated
///   as an empty object). Non-object values are returned unchanged; dynamic
///   merging is skipped and a warning is logged, since the reaction author
///   declared params as something other than a map.
/// * For each `(target_key, source_field)` in `params_from`, the source value
///   is inserted at `target_key`. A `None` source field logs a warning and
///   skips that key — the reaction still fires with a partial param map.
/// * `params` and `params_from` keys must not collide; that is enforced at
///   parse time in `registry::parse_reactions`, so a collision at runtime
///   would indicate a bug. If one is observed here, `params_from` wins and we
///   log an error.
pub(crate) fn build_effective_params(
    target: &ReactionTarget,
    source_entity_id: &str,
    source_fields: &serde_json::Value,
    rule_name: &str,
) -> serde_json::Value {
    if target.params_from.is_empty() {
        return target.params.clone();
    }

    let mut merged = match target.params.clone() {
        serde_json::Value::Object(map) => map,
        serde_json::Value::Null => serde_json::Map::new(),
        other => {
            tracing::warn!(
                rule = rule_name,
                "reaction `params` is not a JSON object; skipping params_from merge"
            );
            return other;
        }
    };

    for (target_key, source_field) in &target.params_from {
        if merged.contains_key(target_key) {
            tracing::error!(
                rule = rule_name,
                key = target_key,
                "reaction params/params_from collision at dispatch time \
                 (should be caught at parse time); params_from wins"
            );
        }

        let source_value = match source_field.as_str() {
            "Id" | "id" => Some(serde_json::Value::String(source_entity_id.to_string())),
            _ => source_fields.get(source_field).cloned(),
        };

        match source_value {
            Some(value) => {
                merged.insert(target_key.clone(), value);
            }
            None => {
                tracing::warn!(
                    rule = rule_name,
                    target_key = target_key,
                    source_field = source_field,
                    "params_from source field missing on source entity; \
                     skipping key (reaction still fires with partial params)"
                );
            }
        }
    }

    serde_json::Value::Object(merged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trigger::types::ReactionTarget;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn target_with(params: serde_json::Value, params_from: Vec<(&str, &str)>) -> ReactionTarget {
        let mut pf = BTreeMap::new();
        for (k, v) in params_from {
            pf.insert(k.to_string(), v.to_string());
        }
        ReactionTarget {
            entity_type: "Target".to_string(),
            action: "Do".to_string(),
            params,
            params_from: pf,
        }
    }

    #[test]
    fn static_only_returns_params_unchanged() {
        let t = target_with(json!({"a": 1}), vec![]);
        let out = build_effective_params(&t, "src-1", &json!({}), "rule");
        assert_eq!(out, json!({"a": 1}));
    }

    #[test]
    fn dynamic_only_reads_from_source_fields() {
        let t = target_with(json!({}), vec![("input", "output")]);
        let fields = json!({"output": "stage-2-payload"});
        let out = build_effective_params(&t, "src-1", &fields, "rule");
        assert_eq!(out, json!({"input": "stage-2-payload"}));
    }

    #[test]
    fn static_and_dynamic_merge_both() {
        let t = target_with(
            json!({"requested_by": "system"}),
            vec![("job_type", "next_stage"), ("input", "output")],
        );
        let fields = json!({"next_stage": "source_search", "output": "payload"});
        let out = build_effective_params(&t, "src-1", &fields, "rule");
        assert_eq!(
            out,
            json!({
                "requested_by": "system",
                "job_type": "source_search",
                "input": "payload",
            })
        );
    }

    #[test]
    fn missing_source_field_skips_key_and_fires_with_partial_params() {
        let t = target_with(json!({"requested_by": "system"}), vec![("input", "output")]);
        let out = build_effective_params(&t, "src-1", &json!({}), "rule");
        assert_eq!(out, json!({"requested_by": "system"}));
    }

    #[test]
    fn null_static_params_treated_as_empty_object() {
        let t = target_with(json!(null), vec![("k", "v")]);
        let fields = json!({"v": "val"});
        let out = build_effective_params(&t, "src-1", &fields, "rule");
        assert_eq!(out, json!({"k": "val"}));
    }

    #[test]
    fn non_object_static_params_bypass_merge() {
        let t = target_with(json!("literal-string"), vec![("k", "v")]);
        let out = build_effective_params(&t, "src-1", &json!({"v": "val"}), "rule");
        assert_eq!(out, json!("literal-string"));
    }

    #[test]
    fn collision_params_from_wins() {
        let t = target_with(json!({"shared": "static"}), vec![("shared", "src")]);
        let fields = json!({"src": "dynamic"});
        let out = build_effective_params(&t, "src-1", &fields, "rule");
        assert_eq!(out, json!({"shared": "dynamic"}));
    }

    #[test]
    fn id_alias_reads_source_entity_id() {
        let t = target_with(json!({}), vec![("last_version_id", "Id")]);
        let out = build_effective_params(&t, "fv-123", &json!({}), "rule");
        assert_eq!(out, json!({"last_version_id": "fv-123"}));
    }
}
