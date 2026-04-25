//! Target entity ID resolution for reaction rules.
//!
//! Shared logic used by both the async [`ReactionDispatcher`] and the
//! simulation [`SimReactionSystem`] to determine which entity instance
//! a reaction rule targets.

use temper_runtime::scheduler::sim_uuid;

use super::types::TargetResolver;

/// Resolve the target entity ID for a reaction rule.
///
/// Evaluates the [`TargetResolver`] variant against the source entity context
/// and transition fields to produce the target entity ID.
///
/// Returns `None` only for `Field` resolvers when the referenced field is
/// absent from the fields payload.
pub(crate) fn resolve_target_id(
    resolver: &TargetResolver,
    source_entity_id: &str,
    fields: &serde_json::Value,
) -> Option<String> {
    match resolver {
        TargetResolver::Field { field } => fields
            .get(field)
            .and_then(|v| v.as_str())
            .filter(|value| !value.is_empty())
            .map(|s| s.to_string()),
        TargetResolver::SameId => Some(source_entity_id.to_string()),
        TargetResolver::Static { entity_id } => Some(entity_id.clone()),
        TargetResolver::CreateIfMissing { id_field } => fields
            .get(id_field)
            .and_then(|v| v.as_str())
            .filter(|value| !value.is_empty())
            .map(|s| s.to_string())
            .or_else(|| Some(format!("{source_entity_id}-derived"))),
        // Fresh UUID on every dispatch. `sim_uuid()` is DST-safe: in
        // simulation it comes from the seeded `DeterministicIdGen`, in
        // production it's `Uuid::now_v7()`. Reaction cascades under
        // `SimReactionSystem` stay reproducible across seeded runs.
        TargetResolver::Create => Some(sim_uuid().to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn field_resolver_extracts_from_fields() {
        let resolver = TargetResolver::Field {
            field: "target_id".to_string(),
        };
        let fields = json!({"target_id": "order-42"});
        assert_eq!(
            resolve_target_id(&resolver, "src-1", &fields),
            Some("order-42".to_string())
        );
    }

    #[test]
    fn field_resolver_returns_none_when_missing() {
        let resolver = TargetResolver::Field {
            field: "missing".to_string(),
        };
        assert_eq!(resolve_target_id(&resolver, "src-1", &json!({})), None);
    }

    #[test]
    fn field_resolver_returns_none_when_field_is_empty() {
        let resolver = TargetResolver::Field {
            field: "workspace_id".to_string(),
        };
        assert_eq!(
            resolve_target_id(&resolver, "src-1", &json!({"workspace_id": ""})),
            None
        );
    }

    #[test]
    fn same_id_returns_source() {
        assert_eq!(
            resolve_target_id(&TargetResolver::SameId, "order-1", &json!({})),
            Some("order-1".to_string())
        );
    }

    #[test]
    fn static_returns_configured_id() {
        let resolver = TargetResolver::Static {
            entity_id: "fixed-42".to_string(),
        };
        assert_eq!(
            resolve_target_id(&resolver, "src-1", &json!({})),
            Some("fixed-42".to_string())
        );
    }

    #[test]
    fn create_if_missing_uses_field_when_present() {
        let resolver = TargetResolver::CreateIfMissing {
            id_field: "new_id".to_string(),
        };
        let fields = json!({"new_id": "explicit-id"});
        assert_eq!(
            resolve_target_id(&resolver, "src-1", &fields),
            Some("explicit-id".to_string())
        );
    }

    #[test]
    fn create_if_missing_derives_when_absent() {
        let resolver = TargetResolver::CreateIfMissing {
            id_field: "new_id".to_string(),
        };
        assert_eq!(
            resolve_target_id(&resolver, "src-1", &json!({})),
            Some("src-1-derived".to_string())
        );
    }

    #[test]
    fn create_resolver_returns_fresh_id_each_call() {
        use temper_runtime::scheduler::install_deterministic_context;
        // Same seed — under the sim context, ids are deterministic but each
        // call advances the seeded generator, so two consecutive calls yield
        // distinct ids.
        let (_guard, _clock, _id_gen) = install_deterministic_context(1234);

        let resolver = TargetResolver::Create;
        let id1 = resolve_target_id(&resolver, "src-1", &json!({})).unwrap();
        let id2 = resolve_target_id(&resolver, "src-1", &json!({})).unwrap();
        assert_ne!(
            id1, id2,
            "Create resolver must produce a fresh id each call"
        );
        assert!(!id1.is_empty());
        assert!(!id2.is_empty());
    }

    #[test]
    fn create_resolver_is_deterministic_across_seeded_runs() {
        use temper_runtime::scheduler::install_deterministic_context;

        let ids_run1 = {
            let (_guard, _clock, _id_gen) = install_deterministic_context(42);
            let resolver = TargetResolver::Create;
            vec![
                resolve_target_id(&resolver, "s", &json!({})).unwrap(),
                resolve_target_id(&resolver, "s", &json!({})).unwrap(),
                resolve_target_id(&resolver, "s", &json!({})).unwrap(),
            ]
        };

        let ids_run2 = {
            let (_guard, _clock, _id_gen) = install_deterministic_context(42);
            let resolver = TargetResolver::Create;
            vec![
                resolve_target_id(&resolver, "s", &json!({})).unwrap(),
                resolve_target_id(&resolver, "s", &json!({})).unwrap(),
                resolve_target_id(&resolver, "s", &json!({})).unwrap(),
            ]
        };

        assert_eq!(
            ids_run1, ids_run2,
            "Create resolver must produce the same id sequence under the same seed"
        );
    }
}
