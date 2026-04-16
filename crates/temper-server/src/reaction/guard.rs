//! Reaction guard evaluation.
//!
//! Guards are evaluated in two passes so production (async) and simulation
//! (sync) share the same traversal:
//!
//! 1. [`collect_cross_entity_queries`] walks the guard tree and emits every
//!    [`super::types::ReactionGuard::CrossEntityStateIn`] query. Leaf
//!    variants that only touch the source entity are *not* emitted — those
//!    are decided later from `source_fields` alone.
//! 2. The caller resolves the queries into a [`CrossStatusMap`] using the
//!    appropriate lookup primitive (async `resolve_entity_status` for
//!    production, synchronous actor-status read for the sim).
//! 3. [`evaluate_with_resolved`] re-walks the same tree using the resolved
//!    map plus `source_fields` to produce the final boolean.
//!
//! This split is the same pattern used at `state/dispatch/cross_entity.rs`
//! for IOA guards (ADR-0015). Reusing it keeps the two worlds consistent
//! and avoids dragging the IOA `Guard` enum's transition-table context into
//! the reaction path.

use std::collections::BTreeMap;

use super::types::ReactionGuard;

/// Unresolved cross-entity query raised by [`collect_cross_entity_queries`].
///
/// The `target_entity_id` is read out of the source entity's fields at
/// collection time so both the production and simulation lookup primitives
/// receive identical inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CrossEntityQuery {
    pub(crate) entity_type: String,
    pub(crate) target_entity_id: String,
    pub(crate) required_status: Vec<String>,
}

impl CrossEntityQuery {
    /// Stable map key used by [`CrossStatusMap`]. Must be a deterministic
    /// function of the query fields so the two passes agree.
    pub(crate) fn key(&self) -> String {
        format!("{}:{}", self.entity_type, self.target_entity_id)
    }

    /// Helper: check a resolved status against `required_status`.
    pub(crate) fn matches(&self, status: &str) -> bool {
        self.required_status.iter().any(|s| s == status)
    }
}

/// Results of cross-entity status lookups, keyed by
/// `"{entity_type}:{target_entity_id}"`. `true` = guard condition satisfied.
/// Missing entries count as `false` during evaluation.
pub(crate) type CrossStatusMap = BTreeMap<String, bool>;

/// First pass — walk the guard tree and collect unresolved cross-entity
/// queries. Queries whose `entity_id_source` field is absent or not a
/// non-empty string are NOT emitted; those branches evaluate to `false`
/// during the second pass (stricter than IOA's vacuous-truth handling — for
/// reactions, an empty target ID is almost always a misconfiguration the
/// author should notice).
pub(crate) fn collect_cross_entity_queries(
    guard: &ReactionGuard,
    source_fields: &serde_json::Value,
    out: &mut Vec<CrossEntityQuery>,
) {
    match guard {
        ReactionGuard::FieldEquals { .. }
        | ReactionGuard::FieldIn { .. }
        | ReactionGuard::BoolTrue { .. }
        | ReactionGuard::BoolFalse { .. }
        | ReactionGuard::StateIn { .. } => {}
        ReactionGuard::CrossEntityStateIn {
            entity_type,
            entity_id_source,
            required_status,
        } => {
            if let Some(target_id) = source_fields
                .get(entity_id_source)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                out.push(CrossEntityQuery {
                    entity_type: entity_type.clone(),
                    target_entity_id: target_id.to_string(),
                    required_status: required_status.clone(),
                });
            }
        }
        ReactionGuard::AllOf { guards } | ReactionGuard::AnyOf { guards } => {
            for g in guards {
                collect_cross_entity_queries(g, source_fields, out);
            }
        }
        ReactionGuard::Not { guard } => {
            collect_cross_entity_queries(guard, source_fields, out);
        }
    }
}

/// Second pass — evaluate the guard using the source fields, the source
/// entity's post-action status, and the resolved cross-entity map.
///
/// Unknown source field → `false`. Unresolved cross-entity query (missing
/// target id, lookup failure) → `false`.
pub(crate) fn evaluate_with_resolved(
    guard: &ReactionGuard,
    source_fields: &serde_json::Value,
    source_status: &str,
    resolved: &CrossStatusMap,
    rule_name: &str,
) -> bool {
    match guard {
        ReactionGuard::FieldEquals { field, value } => match source_fields.get(field) {
            Some(found) => found == value,
            None => {
                tracing::debug!(
                    rule = rule_name,
                    field = field,
                    "reaction guard `field_equals` source field missing — false"
                );
                false
            }
        },
        ReactionGuard::FieldIn { field, values } => match source_fields.get(field) {
            Some(found) => values.iter().any(|v| v == found),
            None => {
                tracing::debug!(
                    rule = rule_name,
                    field = field,
                    "reaction guard `field_in` source field missing — false"
                );
                false
            }
        },
        ReactionGuard::BoolTrue { field } => source_fields
            .get(field)
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        ReactionGuard::BoolFalse { field } => {
            match source_fields.get(field).and_then(|v| v.as_bool()) {
                Some(b) => !b,
                None => false,
            }
        }
        ReactionGuard::StateIn { values } => values.iter().any(|v| v == source_status),
        ReactionGuard::CrossEntityStateIn {
            entity_type,
            entity_id_source,
            ..
        } => {
            let Some(target_id) = source_fields
                .get(entity_id_source)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            else {
                tracing::warn!(
                    rule = rule_name,
                    entity_type = entity_type,
                    entity_id_source = entity_id_source,
                    "reaction guard `cross_entity_state_in` target id missing on source — false"
                );
                return false;
            };
            let key = format!("{entity_type}:{target_id}");
            resolved.get(&key).copied().unwrap_or(false)
        }
        ReactionGuard::AllOf { guards } => guards
            .iter()
            .all(|g| evaluate_with_resolved(g, source_fields, source_status, resolved, rule_name)),
        ReactionGuard::AnyOf { guards } => guards
            .iter()
            .any(|g| evaluate_with_resolved(g, source_fields, source_status, resolved, rule_name)),
        ReactionGuard::Not { guard } => {
            !evaluate_with_resolved(guard, source_fields, source_status, resolved, rule_name)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reaction::types::ReactionGuard;
    use serde_json::json;

    fn empty_map() -> CrossStatusMap {
        CrossStatusMap::new()
    }

    // ---------- source-only leaf evaluation ---------------------------------

    #[test]
    fn field_equals_true_on_match() {
        let g = ReactionGuard::FieldEquals {
            field: "job_type".to_string(),
            value: json!("source_search"),
        };
        let fields = json!({"job_type": "source_search"});
        assert!(evaluate_with_resolved(
            &g,
            &fields,
            "Done",
            &empty_map(),
            "r"
        ));
    }

    #[test]
    fn field_equals_false_on_miss() {
        let g = ReactionGuard::FieldEquals {
            field: "job_type".to_string(),
            value: json!("source_search"),
        };
        assert!(!evaluate_with_resolved(
            &g,
            &json!({}),
            "Done",
            &empty_map(),
            "r"
        ));
        assert!(!evaluate_with_resolved(
            &g,
            &json!({"job_type": "other"}),
            "Done",
            &empty_map(),
            "r"
        ));
    }

    #[test]
    fn field_in_matches_any_of_values() {
        let g = ReactionGuard::FieldIn {
            field: "stage".to_string(),
            values: vec![json!("a"), json!("b")],
        };
        assert!(evaluate_with_resolved(
            &g,
            &json!({"stage": "b"}),
            "Done",
            &empty_map(),
            "r"
        ));
        assert!(!evaluate_with_resolved(
            &g,
            &json!({"stage": "c"}),
            "Done",
            &empty_map(),
            "r"
        ));
    }

    #[test]
    fn bool_true_and_false_variants() {
        let t = ReactionGuard::BoolTrue {
            field: "ready".to_string(),
        };
        let f = ReactionGuard::BoolFalse {
            field: "ready".to_string(),
        };
        assert!(evaluate_with_resolved(
            &t,
            &json!({"ready": true}),
            "X",
            &empty_map(),
            "r"
        ));
        assert!(!evaluate_with_resolved(
            &t,
            &json!({"ready": false}),
            "X",
            &empty_map(),
            "r"
        ));
        assert!(evaluate_with_resolved(
            &f,
            &json!({"ready": false}),
            "X",
            &empty_map(),
            "r"
        ));
        // Missing field → false for both.
        assert!(!evaluate_with_resolved(
            &t,
            &json!({}),
            "X",
            &empty_map(),
            "r"
        ));
        assert!(!evaluate_with_resolved(
            &f,
            &json!({}),
            "X",
            &empty_map(),
            "r"
        ));
    }

    #[test]
    fn state_in_matches_post_status() {
        let g = ReactionGuard::StateIn {
            values: vec!["Complete".to_string(), "Partial".to_string()],
        };
        assert!(evaluate_with_resolved(
            &g,
            &json!({}),
            "Complete",
            &empty_map(),
            "r"
        ));
        assert!(!evaluate_with_resolved(
            &g,
            &json!({}),
            "Failed",
            &empty_map(),
            "r"
        ));
    }

    // ---------- cross-entity collection + resolution ------------------------

    #[test]
    fn cross_entity_collects_and_evaluates() {
        let g = ReactionGuard::CrossEntityStateIn {
            entity_type: "Workspace".to_string(),
            entity_id_source: "workspace_id".to_string(),
            required_status: vec!["Active".to_string()],
        };
        let fields = json!({"workspace_id": "ws-42"});

        let mut queries = Vec::new();
        collect_cross_entity_queries(&g, &fields, &mut queries);
        assert_eq!(queries.len(), 1);
        assert_eq!(queries[0].entity_type, "Workspace");
        assert_eq!(queries[0].target_entity_id, "ws-42");
        assert!(queries[0].matches("Active"));
        assert!(!queries[0].matches("Archived"));
        assert_eq!(queries[0].key(), "Workspace:ws-42");

        // Resolved: Active — guard passes.
        let mut resolved = CrossStatusMap::new();
        resolved.insert("Workspace:ws-42".to_string(), true);
        assert!(evaluate_with_resolved(&g, &fields, "X", &resolved, "r"));

        // Resolved: not Active — guard fails.
        let mut resolved = CrossStatusMap::new();
        resolved.insert("Workspace:ws-42".to_string(), false);
        assert!(!evaluate_with_resolved(&g, &fields, "X", &resolved, "r"));

        // Unresolved (entity fetch failed) — guard fails.
        assert!(!evaluate_with_resolved(&g, &fields, "X", &empty_map(), "r"));
    }

    #[test]
    fn cross_entity_missing_target_id_is_false_and_not_collected() {
        let g = ReactionGuard::CrossEntityStateIn {
            entity_type: "Workspace".to_string(),
            entity_id_source: "workspace_id".to_string(),
            required_status: vec!["Active".to_string()],
        };
        // No workspace_id on source.
        let fields = json!({});
        let mut queries = Vec::new();
        collect_cross_entity_queries(&g, &fields, &mut queries);
        assert!(queries.is_empty(), "no queries collected when id missing");
        assert!(!evaluate_with_resolved(&g, &fields, "X", &empty_map(), "r"));

        // Empty string — also not collected.
        let fields = json!({"workspace_id": ""});
        let mut queries = Vec::new();
        collect_cross_entity_queries(&g, &fields, &mut queries);
        assert!(queries.is_empty());
        assert!(!evaluate_with_resolved(&g, &fields, "X", &empty_map(), "r"));
    }

    // ---------- composite ---------------------------------------------------

    #[test]
    fn all_of_requires_every_child() {
        let g = ReactionGuard::AllOf {
            guards: vec![
                ReactionGuard::BoolTrue {
                    field: "a".to_string(),
                },
                ReactionGuard::BoolTrue {
                    field: "b".to_string(),
                },
            ],
        };
        assert!(evaluate_with_resolved(
            &g,
            &json!({"a": true, "b": true}),
            "X",
            &empty_map(),
            "r"
        ));
        assert!(!evaluate_with_resolved(
            &g,
            &json!({"a": true, "b": false}),
            "X",
            &empty_map(),
            "r"
        ));
    }

    #[test]
    fn any_of_requires_one_child() {
        let g = ReactionGuard::AnyOf {
            guards: vec![
                ReactionGuard::BoolTrue {
                    field: "a".to_string(),
                },
                ReactionGuard::BoolTrue {
                    field: "b".to_string(),
                },
            ],
        };
        assert!(evaluate_with_resolved(
            &g,
            &json!({"a": false, "b": true}),
            "X",
            &empty_map(),
            "r"
        ));
        assert!(!evaluate_with_resolved(
            &g,
            &json!({"a": false, "b": false}),
            "X",
            &empty_map(),
            "r"
        ));
    }

    #[test]
    fn not_inverts() {
        let g = ReactionGuard::Not {
            guard: Box::new(ReactionGuard::BoolTrue {
                field: "a".to_string(),
            }),
        };
        assert!(evaluate_with_resolved(
            &g,
            &json!({"a": false}),
            "X",
            &empty_map(),
            "r"
        ));
        assert!(!evaluate_with_resolved(
            &g,
            &json!({"a": true}),
            "X",
            &empty_map(),
            "r"
        ));
    }

    #[test]
    fn composite_collects_cross_entity_from_all_branches() {
        let g = ReactionGuard::AllOf {
            guards: vec![
                ReactionGuard::CrossEntityStateIn {
                    entity_type: "A".to_string(),
                    entity_id_source: "a_id".to_string(),
                    required_status: vec!["Ok".to_string()],
                },
                ReactionGuard::Not {
                    guard: Box::new(ReactionGuard::CrossEntityStateIn {
                        entity_type: "B".to_string(),
                        entity_id_source: "b_id".to_string(),
                        required_status: vec!["Bad".to_string()],
                    }),
                },
            ],
        };
        let fields = json!({"a_id": "a-1", "b_id": "b-1"});
        let mut queries = Vec::new();
        collect_cross_entity_queries(&g, &fields, &mut queries);
        assert_eq!(queries.len(), 2);
        assert!(queries.iter().any(|q| q.entity_type == "A"));
        assert!(queries.iter().any(|q| q.entity_type == "B"));
    }
}
