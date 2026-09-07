//! Reaction routing derived from the registered actor reaction rules.
use std::collections::HashMap;
use temper_runtime::reaction::ReactionRule;

/// Build per-actor routing maps from reaction rules.
///
/// Returns `HashMap<actor_type, HashMap<emit_name, (target_actor_type, target_action)>>`.
pub fn build_routing_maps(
    rules: &[ReactionRule],
) -> HashMap<String, HashMap<String, (String, String)>> {
    let mut maps: HashMap<String, HashMap<String, (String, String)>> = HashMap::new();

    for rule in rules {
        if let Some(emit_name) = &rule.when.action {
            maps.entry(rule.when.entity_type.clone())
                .or_default()
                .insert(
                    emit_name.clone(),
                    (rule.then.entity_type.clone(), rule.then.action.clone()),
                );
        }
    }

    maps
}

/// Build a single actor's routing map from a reaction registry.
pub fn build_actor_routing(
    actor_type: &str,
    rules: &[ReactionRule],
) -> HashMap<String, (String, String)> {
    rules
        .iter()
        .filter(|r| r.when.entity_type == actor_type)
        .filter_map(|r| {
            r.when.action.as_ref().map(|emit| {
                (
                    emit.clone(),
                    (r.then.entity_type.clone(), r.then.action.clone()),
                )
            })
        })
        .collect()
}
