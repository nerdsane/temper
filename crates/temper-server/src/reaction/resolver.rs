//! Target entity ID resolution for reaction rules.
//!
//! Shared logic used by both the async [`ReactionDispatcher`] and the
//! simulation [`SimReactionSystem`] to determine which entity instance
//! a reaction rule targets.

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
            .map(|s| s.to_string()),
        TargetResolver::SameId => Some(source_entity_id.to_string()),
        TargetResolver::Static { entity_id } => Some(entity_id.clone()),
        TargetResolver::CreateIfMissing { id_field } => fields
            .get(id_field)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| Some(format!("{source_entity_id}-derived"))),
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
}
