//! Relation graph construction and webhook route indexing.

use std::collections::BTreeMap;

use temper_spec::automaton::{ActionTrigger, TriggerGuard, TriggerKind, Webhook};
use temper_spec::cross_invariant::{CrossInvariantSpec, DeletePolicy};
use temper_spec::csdl::CsdlDocument;

use super::types::{EntitySpec, RelationEdge, RelationGraph};
use crate::trigger::types::{
    ReactionGuard, ReactionRule, ReactionTarget, ReactionTrigger, TargetResolver,
};

/// Build webhook route index from parsed entity specs.
pub(super) fn build_webhook_routes(
    entities: &BTreeMap<String, EntitySpec>,
) -> BTreeMap<String, (String, Webhook)> {
    let mut routes = BTreeMap::new();
    for (entity_type, spec) in entities {
        for wh in &spec.automaton.webhooks {
            routes.insert(wh.path.clone(), (entity_type.clone(), wh.clone()));
        }
    }
    routes
}

/// Build a relation graph from the CSDL and optional cross-invariant overrides.
pub(super) fn build_relation_graph(
    csdl: &CsdlDocument,
    cross_invariants: Option<&CrossInvariantSpec>,
) -> RelationGraph {
    let mut overrides = BTreeMap::<(String, String), DeletePolicy>::new();
    let default_policy = cross_invariants
        .map(|spec| {
            for ov in &spec.relation_overrides {
                overrides.insert(
                    (ov.from_entity.clone(), ov.navigation_property.clone()),
                    ov.delete_policy,
                );
            }
            spec.default_delete_policy
        })
        .unwrap_or(DeletePolicy::Restrict);

    let mut graph = RelationGraph::default();
    for schema in &csdl.schemas {
        for et in &schema.entity_types {
            for nav in &et.navigation_properties {
                let target = nav_target_entity(&nav.type_name);
                for rc in &nav.referential_constraints {
                    let delete_policy = overrides
                        .get(&(et.name.clone(), nav.name.clone()))
                        .copied()
                        .unwrap_or(default_policy);
                    let edge = RelationEdge {
                        from_entity: et.name.clone(),
                        navigation_property: nav.name.clone(),
                        to_entity: target.clone(),
                        source_field: rc.property.clone(),
                        target_field: rc.referenced_property.clone(),
                        nullable: nav.nullable,
                        delete_policy,
                    };
                    graph
                        .outgoing
                        .entry(et.name.clone())
                        .or_default()
                        .push(edge.clone());
                    graph.incoming.entry(target.clone()).or_default().push(edge);
                }
            }
        }
    }
    graph
}

/// Extract the target entity type name from a CSDL navigation type string.
fn nav_target_entity(type_name: &str) -> String {
    let raw = type_name.trim();
    let inner = if raw.starts_with("Collection(") && raw.ends_with(')') {
        &raw[11..raw.len() - 1]
    } else {
        raw
    };
    inner.rsplit('.').next().unwrap_or(inner).to_string()
}

// ADR-0046: `synthesize_agent_trigger_reactions` removed. Agent spawning
// is now an `[[action.triggers]]` block with kind="entity"; auto-start-on-
// Assign behavior lives on the target agent entity's own spec as a
// self-trigger (see ADR-0046 Sub-Decision 7).

/// Synthesize a `ReactionRule` from an `[[action.triggers]]` entry (ADR-0046).
///
/// Returns `None` for `kind = "wasm"` and `kind = "webhook"` triggers — those
/// are handled by a separate runtime path in a later slice. Returns `Some`
/// for `kind = "entity"` triggers, translating the declaration into the
/// existing reaction machinery, including the declared trigger principal.
///
/// Guard translation: `TriggerGuard` and `ReactionGuard` are structurally
/// identical enums living in different crates (spec vs server layer). The
/// converter is a pure one-to-one mapping.
pub(super) fn synthesize_action_trigger_reaction(
    source_entity_type: &str,
    source_action: &str,
    trigger: &ActionTrigger,
) -> Option<ReactionRule> {
    // Only entity-kind triggers map to ReactionRules. Wasm / Webhook
    // triggers have a different runtime (deferred to a later slice).
    if trigger.kind != TriggerKind::Entity {
        return None;
    }

    // Entity-kind triggers have been validated to have all three fields
    // populated at parse time (see temper-spec parser::validate_action_triggers).
    // Defensive `?` here guards against future parser changes.
    let target_entity = trigger.target_entity.clone()?;
    let target_action = trigger.target_action.clone()?;
    let resolve_target = trigger.resolve_target.clone()?;

    Some(ReactionRule {
        name: format!("{source_entity_type}:{source_action}:{}", trigger.name),
        when: ReactionTrigger {
            entity_type: source_entity_type.to_string(),
            action: Some(source_action.to_string()),
            to_state: trigger.to_state.clone(),
            guard: trigger.guard.as_ref().map(trigger_guard_to_reaction_guard),
        },
        then: ReactionTarget {
            entity_type: target_entity,
            action: target_action,
            params: if trigger.params.is_null() {
                serde_json::json!({})
            } else {
                trigger.params.clone()
            },
            params_from: trigger.params_from.clone(),
        },
        resolve_target: target_resolver_to_target_resolver(&resolve_target),
        principal: trigger.principal.clone(),
        drop_ok: trigger.drop_ok,
    })
}

/// Convert a [`temper_spec::automaton::TargetResolver`] to a
/// [`crate::trigger::types::TargetResolver`]. Structurally identical; the
/// two enums exist because the spec layer can't depend on the server layer.
fn target_resolver_to_target_resolver(
    spec_resolver: &temper_spec::automaton::TargetResolver,
) -> TargetResolver {
    use temper_spec::automaton::TargetResolver as Spec;
    match spec_resolver {
        Spec::Field { field } => TargetResolver::Field {
            field: field.clone(),
        },
        Spec::SameId => TargetResolver::SameId,
        Spec::Static { entity_id } => TargetResolver::Static {
            entity_id: entity_id.clone(),
        },
        Spec::CreateIfMissing { id_field } => TargetResolver::CreateIfMissing {
            id_field: id_field.clone(),
        },
        Spec::Create => TargetResolver::Create,
    }
}

/// Convert a [`TriggerGuard`] (spec layer) into a [`ReactionGuard`] (server
/// layer). Structurally identical enums; the mapping is mechanical.
fn trigger_guard_to_reaction_guard(g: &TriggerGuard) -> ReactionGuard {
    match g {
        TriggerGuard::FieldEquals { field, value } => ReactionGuard::FieldEquals {
            field: field.clone(),
            value: value.clone(),
        },
        TriggerGuard::FieldIn { field, values } => ReactionGuard::FieldIn {
            field: field.clone(),
            values: values.clone(),
        },
        TriggerGuard::BoolTrue { field } => ReactionGuard::BoolTrue {
            field: field.clone(),
        },
        TriggerGuard::BoolFalse { field } => ReactionGuard::BoolFalse {
            field: field.clone(),
        },
        TriggerGuard::StateIn { values } => ReactionGuard::StateIn {
            values: values.clone(),
        },
        TriggerGuard::CrossEntityStateIn {
            entity_type,
            entity_id_source,
            required_status,
        } => ReactionGuard::CrossEntityStateIn {
            entity_type: entity_type.clone(),
            entity_id_source: entity_id_source.clone(),
            required_status: required_status.clone(),
        },
        TriggerGuard::AllOf { guards } => ReactionGuard::AllOf {
            guards: guards.iter().map(trigger_guard_to_reaction_guard).collect(),
        },
        TriggerGuard::AnyOf { guards } => ReactionGuard::AnyOf {
            guards: guards.iter().map(trigger_guard_to_reaction_guard).collect(),
        },
        TriggerGuard::Not { guard } => ReactionGuard::Not {
            guard: Box::new(trigger_guard_to_reaction_guard(guard)),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nav_target_simple_type() {
        assert_eq!(nav_target_entity("Order"), "Order");
    }

    // ─── ADR-0046: action trigger synthesis tests ────────────────────────

    #[test]
    fn synthesize_entity_trigger_emits_reaction_rule() {
        let trigger = temper_spec::automaton::ActionTrigger {
            name: "create_version".to_string(),
            kind: TriggerKind::Entity,
            principal: Some("file-service".to_string()),
            to_state: Some("Ready".to_string()),
            guard: None,
            liveness: temper_spec::automaton::TriggerLiveness::BestEffort,
            drop_ok: true,
            llm: false,
            target_entity: Some("FileVersion".to_string()),
            target_action: Some("Create".to_string()),
            params: serde_json::json!({}),
            params_from: std::collections::BTreeMap::new(),
            resolve_target: Some(temper_spec::automaton::TargetResolver::CreateIfMissing {
                id_field: "last_version_id".to_string(),
            }),
            module: None,
            on_success: None,
            on_failure: None,
            config: std::collections::BTreeMap::new(),
            adapter: None,
            adapter_type: None,
            url: None,
            method: None,
            headers: std::collections::BTreeMap::new(),
            body_template: None,
        };

        let rule = synthesize_action_trigger_reaction("File", "StreamUpdated", &trigger)
            .expect("entity kind should synthesize");
        assert_eq!(rule.name, "File:StreamUpdated:create_version");
        assert_eq!(rule.when.entity_type, "File");
        assert_eq!(rule.when.action.as_deref(), Some("StreamUpdated"));
        assert_eq!(rule.when.to_state.as_deref(), Some("Ready"));
        assert_eq!(rule.then.entity_type, "FileVersion");
        assert_eq!(rule.then.action, "Create");
        assert!(rule.drop_ok, "drop_ok must survive trigger normalization");
        assert!(matches!(
            rule.resolve_target,
            TargetResolver::CreateIfMissing { .. }
        ));
    }

    #[test]
    fn synthesize_wasm_trigger_returns_none() {
        let trigger = temper_spec::automaton::ActionTrigger {
            name: "charge".to_string(),
            kind: TriggerKind::Wasm,
            principal: None,
            to_state: None,
            guard: None,
            liveness: temper_spec::automaton::TriggerLiveness::BestEffort,
            drop_ok: false,
            llm: false,
            target_entity: None,
            target_action: None,
            params: serde_json::json!({}),
            params_from: std::collections::BTreeMap::new(),
            resolve_target: None,
            module: Some("stripe".to_string()),
            on_success: None,
            on_failure: None,
            config: std::collections::BTreeMap::new(),
            adapter: None,
            adapter_type: None,
            url: None,
            method: None,
            headers: std::collections::BTreeMap::new(),
            body_template: None,
        };
        assert!(synthesize_action_trigger_reaction("Order", "Confirm", &trigger).is_none());
    }

    #[test]
    fn synthesize_webhook_trigger_returns_none() {
        let trigger = temper_spec::automaton::ActionTrigger {
            name: "notify".to_string(),
            kind: TriggerKind::Webhook,
            principal: None,
            to_state: None,
            guard: None,
            liveness: temper_spec::automaton::TriggerLiveness::BestEffort,
            drop_ok: false,
            llm: false,
            target_entity: None,
            target_action: None,
            params: serde_json::json!({}),
            params_from: std::collections::BTreeMap::new(),
            resolve_target: None,
            module: None,
            on_success: None,
            on_failure: None,
            config: std::collections::BTreeMap::new(),
            adapter: None,
            adapter_type: None,
            url: Some("https://example.com".to_string()),
            method: Some("POST".to_string()),
            headers: std::collections::BTreeMap::new(),
            body_template: None,
        };
        assert!(synthesize_action_trigger_reaction("Order", "Confirm", &trigger).is_none());
    }

    #[test]
    fn nav_target_qualified_type() {
        assert_eq!(nav_target_entity("MyNamespace.Order"), "Order");
    }

    #[test]
    fn nav_target_collection_type() {
        assert_eq!(
            nav_target_entity("Collection(MyNamespace.OrderItem)"),
            "OrderItem"
        );
    }

    #[test]
    fn nav_target_collection_simple() {
        assert_eq!(nav_target_entity("Collection(Item)"), "Item");
    }

    #[test]
    fn nav_target_whitespace_trimmed() {
        assert_eq!(nav_target_entity("  Order  "), "Order");
    }
}
