//! Shared helper for building a reaction's params from its `args`.
//!
//! Used by both the async [`super::dispatcher::ReactionDispatcher`] and the
//! deterministic [`super::sim_dispatcher::SimReactionSystem`] so production
//! and simulation apply identical semantics.

use super::types::ReactionTarget;

/// Build the params for a reaction dispatch from `target.args`, read against
/// the source entity (see [`temper_spec::automaton::resolve_args`]).
///
/// A literal passes its value, `Id` the source entity's id, and a field name
/// that field of `source_fields`, which are the source's fields as guards read
/// them: a declared variable it stores no value for has its initial value. A
/// field that leaves absent, such as a parameter the source action was not
/// sent, logs a warning and leaves its key out — the reaction still fires with
/// the other params.
pub(crate) fn build_effective_params(
    target: &ReactionTarget,
    source_entity_id: &str,
    source_fields: &serde_json::Value,
    rule_name: &str,
) -> serde_json::Value {
    let (params, missing) =
        temper_spec::automaton::resolve_args(&target.args, source_entity_id, source_fields);
    for field in missing {
        tracing::warn!(
            rule = rule_name,
            source_field = field,
            "reaction arg reads a field missing on the source entity; \
             skipping key (reaction still fires with partial params)"
        );
    }
    serde_json::Value::Object(params)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trigger::types::ReactionTarget;
    use serde_json::json;
    use temper_spec::predicate::parse_arg;

    fn target_with(args: &[(&str, &str)]) -> ReactionTarget {
        ReactionTarget {
            entity_type: "Target".to_string(),
            action: "Do".to_string(),
            args: args
                .iter()
                .map(|(k, v)| (k.to_string(), parse_arg(v).unwrap()))
                .collect(),
        }
    }

    #[test]
    fn literals_pass_their_values() {
        let t = target_with(&[("a", "1"), ("b", "'x'"), ("c", "true"), ("d", "null")]);
        let out = build_effective_params(&t, "src-1", &json!({}), "rule");
        assert_eq!(out, json!({"a": 1, "b": "x", "c": true, "d": null}));
    }

    #[test]
    fn names_read_source_fields() {
        let t = target_with(&[("input", "output")]);
        let fields = json!({"output": "stage-2-payload"});
        let out = build_effective_params(&t, "src-1", &fields, "rule");
        assert_eq!(out, json!({"input": "stage-2-payload"}));
    }

    #[test]
    fn literals_and_fields_combine() {
        let t = target_with(&[
            ("requested_by", "'system'"),
            ("job_type", "next_stage"),
            ("input", "output"),
        ]);
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
        let t = target_with(&[("requested_by", "'system'"), ("input", "output")]);
        let out = build_effective_params(&t, "src-1", &json!({}), "rule");
        assert_eq!(out, json!({"requested_by": "system"}));
    }

    #[test]
    fn no_args_is_an_empty_object() {
        let out = build_effective_params(&target_with(&[]), "src-1", &json!({}), "rule");
        assert_eq!(out, json!({}));
    }

    #[test]
    fn id_reads_source_entity_id() {
        let t = target_with(&[("last_version_id", "Id"), ("alias", "id")]);
        let out = build_effective_params(&t, "fv-123", &json!({"Id": "other"}), "rule");
        assert_eq!(out, json!({"last_version_id": "fv-123", "alias": "fv-123"}));
    }
}
