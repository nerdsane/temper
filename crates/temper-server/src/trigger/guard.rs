//! Reaction guard evaluation.
//!
//! A reaction guard is a [`temper_spec::predicate::Expr`] over the source
//! entity's post-action fields and status. Guards are evaluated in two passes
//! so production (async) and simulation (sync) share the same traversal:
//!
//! 1. [`related_ids`] lists every related-entity reference the guard reads,
//!    with the ids found in the source fields.
//! 2. The caller resolves each id's status into a [`RelatedMap`] using the
//!    appropriate lookup primitive (async `resolve_entity_status` for
//!    production, synchronous actor-status read for the sim).
//! 3. [`guard_holds`] evaluates the guard against the fields, the status and
//!    the resolved statuses.
//!
//! This is the same split `state/dispatch/cross_entity.rs` uses for action
//! guards. An unset reference, or a related entity that is not found, reads
//! as `null`.

use temper_jit::table::{Related, RelatedMap};
use temper_spec::predicate::{Env, Expr, Truth, Val, eval};

/// Every `(entity_type, id_field)` the guard reads, with the non-empty ids
/// in that field (one id, or a list of ids).
pub(crate) fn related_ids(
    guard: &Expr,
    fields: &serde_json::Value,
) -> Vec<((String, String), Vec<String>)> {
    guard
        .cross_refs()
        .into_iter()
        .map(|(entity_type, id_field)| {
            let ids = match fields.get(&id_field) {
                Some(serde_json::Value::String(id)) if !id.is_empty() => vec![id.clone()],
                Some(serde_json::Value::Array(items)) => items
                    .iter()
                    .filter_map(|item| item.as_str())
                    .filter(|id| !id.is_empty())
                    .map(str::to_string)
                    .collect(),
                _ => Vec::new(),
            };
            ((entity_type, id_field), ids)
        })
        .collect()
}

struct ReactionEnv<'a> {
    fields: &'a serde_json::Value,
    status: &'a str,
    related: &'a RelatedMap,
}

impl Env for ReactionEnv<'_> {
    fn status(&self) -> Val<'_> {
        Val::Str(self.status)
    }

    fn var(&self, name: &str) -> Val<'_> {
        self.fields.get(name).map_or(Val::Null, Val::json)
    }

    fn cross_statuses(&self, entity_type: &str, id_field: &str) -> Option<Vec<Val<'_>>> {
        let key = (entity_type.to_string(), id_field.to_string());
        match self.related.get(&key) {
            Some(Related::Unresolved) => None,
            Some(Related::Statuses(statuses)) if !statuses.is_empty() => Some(
                statuses
                    .iter()
                    .map(|status| status.as_deref().map_or(Val::Null, Val::Str))
                    .collect(),
            ),
            _ => Some(vec![Val::Null]),
        }
    }
}

/// Whether a reaction guard holds for the source entity's post-action
/// `fields` and `status`, given the resolved related-entity statuses.
pub(crate) fn guard_holds(
    guard: &Expr,
    fields: &serde_json::Value,
    status: &str,
    related: &RelatedMap,
) -> bool {
    let env = ReactionEnv {
        fields,
        status,
        related,
    };
    eval(guard, &env) == Truth::True
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn holds(source: &str, fields: serde_json::Value, related: &RelatedMap) -> bool {
        let guard = temper_spec::predicate::parse(source).unwrap();
        guard_holds(&guard, &fields, "Ready", related)
    }

    #[test]
    fn reads_fields_and_post_status() {
        let none = RelatedMap::new();
        let fields = json!({ "job_type": "export", "paid": true, "count": 3 });
        assert!(holds(
            "job_type == 'export' && paid && count >= 3",
            fields.clone(),
            &none
        ));
        assert!(holds(
            "job_type in ['import', 'export']",
            fields.clone(),
            &none
        ));
        assert!(holds("status == 'Ready'", fields.clone(), &none));
        assert!(!holds("status in ['Draft']", fields.clone(), &none));
        // An absent field is null: not true, not an explicit false.
        assert!(!holds("missing", fields.clone(), &none));
        assert!(!holds("missing == false", fields.clone(), &none));
        assert!(holds("!missing && missing == null", fields, &none));
    }

    #[test]
    fn related_statuses_come_from_the_resolved_map() {
        let fields = json!({ "parent_id": "p-1", "child_ids": ["c-1", "c-2"], "none": "" });
        let guard = temper_spec::predicate::parse(
            "Parent[parent_id].status == 'Active' && Child[child_ids].status == 'Done' && Other[none].status == null",
        )
        .unwrap();
        assert_eq!(
            related_ids(&guard, &fields),
            vec![
                (("Parent".into(), "parent_id".into()), vec!["p-1".into()]),
                (
                    ("Child".into(), "child_ids".into()),
                    vec!["c-1".into(), "c-2".into()]
                ),
                (("Other".into(), "none".into()), vec![]),
            ]
        );
        let mut related = RelatedMap::new();
        related.insert(
            ("Parent".into(), "parent_id".into()),
            Related::Statuses(vec![Some("Active".into())]),
        );
        related.insert(
            ("Child".into(), "child_ids".into()),
            Related::Statuses(vec![Some("Done".into()), Some("Done".into())]),
        );
        assert!(guard_holds(&guard, &fields, "Ready", &related));
        // Every related entity must pass; a missing one reads as null.
        related.insert(
            ("Child".into(), "child_ids".into()),
            Related::Statuses(vec![Some("Done".into()), None]),
        );
        assert!(!guard_holds(&guard, &fields, "Ready", &related));
    }
}
