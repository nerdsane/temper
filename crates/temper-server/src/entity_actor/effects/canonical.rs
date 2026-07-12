//! Canonical JIT state adapter used by every server execution path.

use temper_jit::table::{EffectState, EvalContext};

use super::super::types::EntityState;

impl EffectState for EntityState {
    fn status(&self) -> &str {
        &self.status
    }

    fn status_mut(&mut self) -> &mut String {
        &mut self.status
    }

    fn legacy_item_count(&self) -> Option<usize> {
        Some(self.item_count)
    }

    fn legacy_item_count_mut(&mut self) -> Option<&mut usize> {
        Some(&mut self.item_count)
    }

    fn counters(&self) -> &std::collections::BTreeMap<String, usize> {
        &self.counters
    }

    fn counters_mut(&mut self) -> &mut std::collections::BTreeMap<String, usize> {
        &mut self.counters
    }

    fn booleans(&self) -> &std::collections::BTreeMap<String, bool> {
        &self.booleans
    }

    fn booleans_mut(&mut self) -> &mut std::collections::BTreeMap<String, bool> {
        &mut self.booleans
    }

    fn lists(&self) -> &std::collections::BTreeMap<String, Vec<String>> {
        &self.lists
    }

    fn lists_mut(&mut self) -> &mut std::collections::BTreeMap<String, Vec<String>> {
        &mut self.lists
    }

    fn fields(&self) -> &serde_json::Value {
        &self.fields
    }

    fn fields_mut(&mut self) -> &mut serde_json::Value {
        &mut self.fields
    }
}

/// Maximum cross-entity lookups per transition (TigerStyle budget).
///
/// Rich contracts may legitimately verify several sibling entities in one
/// guarded transition.
pub const MAX_CROSS_ENTITY_LOOKUPS: usize = 16;
/// Maximum entity spawns per transition (TigerStyle budget).
pub const MAX_SPAWNS_PER_TRANSITION: usize = 8;

/// Build an [`EvalContext`] from current entity state.
///
/// This is the single source of truth for context construction. All code paths
/// that call `TransitionTable::evaluate_ctx()` MUST use this function.
pub fn build_eval_context(state: &EntityState) -> EvalContext {
    build_eval_context_with_xref(state, &std::collections::BTreeMap::new())
}

/// Build an [`EvalContext`] with pre-resolved cross-entity booleans.
///
/// The `cross_entity_booleans` map contains `__xref:{type}:{field} -> bool` entries
/// from cross-entity state gate resolution at the dispatch layer.
pub fn build_eval_context_with_xref(
    state: &EntityState,
    cross_entity_booleans: &std::collections::BTreeMap<String, bool>,
) -> EvalContext {
    let mut ctx = temper_jit::table::build_effect_eval_context(state);
    // Merge pre-resolved cross-entity state booleans
    for (k, v) in cross_entity_booleans {
        ctx.booleans.insert(k.clone(), *v);
    }
    ctx
}
