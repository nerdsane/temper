//! Routing derived from a spec's entity-kind `[[action.triggers]]`.
use std::collections::BTreeMap;

use temper_spec::automaton::{Automaton, TargetResolver, TriggerKind};

/// Per-action routes: `action name → [(target actor type, target action)]`.
///
/// Actors address siblings in their own namespace, so only `same_id`
/// entity triggers route here; other resolvers are skipped with a warning.
pub fn build_actor_routing(automaton: &Automaton) -> BTreeMap<String, Vec<(String, String)>> {
    let mut routes: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for action in &automaton.actions {
        for trigger in &action.triggers {
            if trigger.kind != TriggerKind::Entity {
                continue;
            }
            let (Some(target_entity), Some(target_action)) =
                (&trigger.target_entity, &trigger.target_action)
            else {
                continue;
            };
            if trigger.resolve_target != Some(TargetResolver::SameId) {
                tracing::warn!(
                    actor = %automaton.automaton.name,
                    action = %action.name,
                    trigger = %trigger.name,
                    "actor runtime routes only same_id entity triggers; trigger skipped"
                );
                continue;
            }
            routes
                .entry(action.name.clone())
                .or_default()
                .push((target_entity.clone(), target_action.clone()));
        }
    }
    routes
}
