//! Relation graph construction and webhook route indexing.

use std::collections::BTreeMap;

use temper_spec::automaton::{AgentTrigger, Webhook};
use temper_spec::cross_invariant::{CrossInvariantSpec, DeletePolicy};
use temper_spec::csdl::CsdlDocument;

use super::types::{EntitySpec, RelationEdge, RelationGraph};
use crate::reaction::types::{ReactionRule, ReactionTarget, ReactionTrigger, TargetResolver};

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

/// Synthesize reaction rules from an `[[agent_trigger]]` section.
///
/// Produces two rules:
/// 1. Source action -> create + assign an Agent entity
/// 2. Agent.Assign -> Agent.Start (auto-start the assigned agent)
pub(super) fn synthesize_agent_trigger_reactions(
    entity_type: &str,
    trigger: &AgentTrigger,
) -> Vec<ReactionRule> {
    let model = trigger
        .agent_model
        .clone()
        .unwrap_or_else(|| "claude-sonnet-4-6".to_string());
    let agent_type_id = trigger.agent_type_id.clone().unwrap_or_default();

    let mut params = serde_json::json!({
        "role": trigger.agent_role,
        "goal": trigger.agent_goal,
        "model": model,
    });
    if !agent_type_id.is_empty() {
        params["agent_type_id"] = serde_json::Value::String(agent_type_id);
    }

    vec![
        // Rule 1: Source action -> create + assign Agent
        ReactionRule {
            name: format!("{}:agent_trigger:{}", entity_type, trigger.name),
            when: ReactionTrigger {
                entity_type: entity_type.to_string(),
                action: Some(trigger.on_action.clone()),
                to_state: trigger.to_state.clone(),
            },
            then: ReactionTarget {
                entity_type: "Agent".to_string(),
                action: "Assign".to_string(),
                params,
            },
            resolve_target: TargetResolver::CreateIfMissing {
                id_field: "id".to_string(),
            },
        },
        // Rule 2: Agent.Assign -> Agent.Start (auto-start the assigned agent)
        ReactionRule {
            name: format!("{}:agent_trigger:{}:start", entity_type, trigger.name),
            when: ReactionTrigger {
                entity_type: "Agent".to_string(),
                action: Some("Assign".to_string()),
                to_state: Some("Assigned".to_string()),
            },
            then: ReactionTarget {
                entity_type: "Agent".to_string(),
                action: "Start".to_string(),
                params: serde_json::json!({}),
            },
            resolve_target: TargetResolver::SameId,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nav_target_simple_type() {
        assert_eq!(nav_target_entity("Order"), "Order");
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
