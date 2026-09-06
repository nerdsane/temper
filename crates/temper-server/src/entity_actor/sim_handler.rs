//! Simulation handler for entity actors.
//!
//! [`EntityActorHandler`] wraps a real [`TransitionTable`] and [`EntityState`],
//! implementing [`SimActorHandler`] for deterministic simulation. The
//! `handle_message()` method is the synchronous subset of the production
//! `EntityActor::handle()`: same `evaluate()` call, same effect application,
//! same event recording. No async, no persistence, no telemetry.

use futures_util::FutureExt;
use std::sync::Arc;

use temper_jit::table::{EvalContext, TransitionTable};
use temper_runtime::scheduler::{CompareOp, SimActorHandler, SpecAssert, SpecInvariant};
use temper_spec::automaton::StateVar;

use super::effects::{FieldSyncMode, ScheduledAction};
use super::types::EntityState;

/// Simulation handler wrapping a real TransitionTable.
///
/// This is the bridge that lets [`SimActorSystem`] exercise the identical
/// `TransitionTable::evaluate()` path used in production, with deterministic
/// clock and ID generation.
pub struct EntityActorHandler {
    table: Arc<TransitionTable>,
    state: EntityState,
    invariants: Vec<SpecInvariant>,
    /// Custom effects from the last successful action (integration triggers).
    last_custom_effects: Vec<String>,
    /// Scheduled actions from the last successful action (timer requests).
    last_scheduled_actions: Vec<ScheduledAction>,
    field_sync_mode: FieldSyncMode,
    overflow_blobs: Vec<crate::blobs::OverflowBlobWrite>,
}

impl EntityActorHandler {
    /// Create a new simulation handler for an entity.
    pub fn new(
        entity_type: impl Into<String>,
        entity_id: impl Into<String>,
        table: Arc<TransitionTable>,
    ) -> Self {
        let entity_type = entity_type.into();
        let entity_id = entity_id.into();
        let state = super::actor::EntityActor::build_initial_state(
            &entity_type,
            &entity_id,
            &table,
            &serde_json::json!({}),
        );

        Self {
            table,
            state,
            invariants: Vec::new(),
            last_custom_effects: Vec::new(),
            last_scheduled_actions: Vec::new(),
            field_sync_mode: FieldSyncMode::InlineTruncate,
            overflow_blobs: Vec::new(),
        }
    }

    /// Select the production storage representation exercised by this simulation.
    pub fn with_field_sync_mode(mut self, mode: FieldSyncMode) -> Self {
        self.field_sync_mode = mode;
        self
    }

    /// Build an [`EvalContext`] from the current entity state.
    fn eval_context(&self) -> EvalContext {
        super::effects::build_eval_context(&self.state)
    }

    /// Attach spec invariants parsed from I/O Automaton TOML source.
    ///
    /// The [`SimActorSystem`] checks these automatically after every
    /// successful transition — no manual `set_invariant_checker()` needed.
    pub fn with_ioa_invariants(mut self, ioa_toml: &str) -> Self {
        let automaton = temper_spec::automaton::parse_automaton(ioa_toml)
            .expect("failed to parse I/O Automaton TOML for invariants");
        let declared_bools: std::collections::BTreeSet<_> = automaton
            .state
            .iter()
            .filter(|state| is_declared_bool(state))
            .map(|state| state.name.clone())
            .collect();

        self.invariants = automaton
            .invariants
            .iter()
            .filter_map(|inv| {
                let assert_kind = parse_assert_expr(&inv.assert, &declared_bools)?;
                Some(SpecInvariant {
                    name: inv.name.clone(),
                    when: inv.when.clone(),
                    assert: assert_kind,
                })
            })
            .collect();

        self
    }
}

/// Map a shared [`ParsedAssert`] to the runtime [`SpecAssert`].
///
/// Uses [`temper_spec::automaton::parse_assert_expr`] as the single parser,
/// then maps the result to the runtime type. Returns `None` for expressions
/// that the framework cannot check automatically.
fn parse_assert_expr(
    expr: &str,
    declared_bools: &std::collections::BTreeSet<String>,
) -> Option<SpecAssert> {
    use temper_spec::automaton::parse_assert_expr as parse;
    translate_parsed(parse(expr)?, declared_bools)
}

fn translate_parsed(
    parsed: temper_spec::automaton::ParsedAssert,
    declared_bools: &std::collections::BTreeSet<String>,
) -> Option<SpecAssert> {
    use temper_spec::automaton::{AssertCompareOp, ParsedAssert};

    match parsed {
        ParsedAssert::CounterPositive { var } => Some(SpecAssert::CounterPositive { var }),
        ParsedAssert::NoFurtherTransitions => Some(SpecAssert::NoFurtherTransitions),
        ParsedAssert::OrderingConstraint { before, after } => {
            Some(SpecAssert::OrderingConstraint { before, after })
        }
        ParsedAssert::NeverState { state } => Some(SpecAssert::NeverState { state }),
        ParsedAssert::CounterCompare { var, op, value } => {
            let runtime_op = match op {
                AssertCompareOp::Gt => CompareOp::Gt,
                AssertCompareOp::Gte => CompareOp::Gte,
                AssertCompareOp::Lt => CompareOp::Lt,
                AssertCompareOp::Lte => CompareOp::Lte,
                AssertCompareOp::Eq => CompareOp::Eq,
            };
            Some(SpecAssert::CounterCompare {
                var,
                op: runtime_op,
                value,
            })
        }
        ParsedAssert::BoolRequired { var, expect } => declared_bools
            .contains(&var)
            .then_some(SpecAssert::BoolRequired { var, expect }),
        ParsedAssert::And(parts) => {
            let mapped: Option<Vec<_>> = parts
                .into_iter()
                .map(|part| translate_parsed(part, declared_bools))
                .collect();
            mapped.map(SpecAssert::And)
        }
        ParsedAssert::Or(parts) => {
            let mapped: Option<Vec<_>> = parts
                .into_iter()
                .map(|part| translate_parsed(part, declared_bools))
                .collect();
            mapped.map(SpecAssert::Or)
        }
    }
}

fn is_declared_bool(state: &StateVar) -> bool {
    state.var_type == "bool"
}

fn retain_current_blobs(
    fields: &serde_json::Value,
    blobs: &mut Vec<crate::blobs::OverflowBlobWrite>,
) {
    fn collect(value: &serde_json::Value, keys: &mut std::collections::BTreeSet<String>) {
        if let Some(descriptor) = crate::blobs::field_overflow_descriptor(value) {
            keys.insert(descriptor.key.to_owned());
        } else {
            match value {
                serde_json::Value::Object(fields) => {
                    fields.values().for_each(|value| collect(value, keys))
                }
                serde_json::Value::Array(values) => {
                    values.iter().for_each(|value| collect(value, keys))
                }
                _ => {}
            }
        }
    }
    let mut keys = std::collections::BTreeSet::new();
    collect(fields, &mut keys);
    // Removing a key after retention deduplicates content-addressed writes.
    blobs.retain(|blob| keys.remove(&blob.key));
}

impl SimActorHandler for EntityActorHandler {
    fn init(&mut self) -> Result<serde_json::Value, String> {
        self.state = super::actor::EntityActor::build_initial_state(
            &self.state.entity_type,
            &self.state.entity_id,
            &self.table,
            &serde_json::json!({}),
        );

        self.overflow_blobs.clear();
        self.last_custom_effects.clear();
        self.last_scheduled_actions.clear();
        Ok(serde_json::to_value(&self.state).unwrap_or_default())
    }

    fn handle_message(&mut self, action: &str, params: &str) -> Result<serde_json::Value, String> {
        let params_value: serde_json::Value = serde_json::from_str(params)
            .map_err(|_| "Action parameters must contain valid JSON".to_owned())?;

        // The memory-only source must complete synchronously. Any accidental I/O
        // in this path is a simulator invariant failure, not a second interpreter.
        let result = super::action_input::process_action_with_blob_prestate(
            &mut self.state,
            &self.table,
            action,
            &params_value,
            &std::collections::BTreeMap::new(),
            self.field_sync_mode,
            crate::blobs::BlobReadSource::Staged {
                store: None,
                legacy: None,
                blobs: &self.overflow_blobs,
            },
        )
        .now_or_never()
        .expect("simulation blob reads must be memory-only");

        if result.success {
            self.overflow_blobs.extend(result.overflow_blobs);
            retain_current_blobs(&self.state.fields, &mut self.overflow_blobs);
            // Capture custom effects for integration callback scheduling
            self.last_custom_effects = result.custom_effects;
            self.last_scheduled_actions = result.scheduled_actions;
            if let Some(event) = result.event {
                self.state.push_event_bounded(event);
            }
            Ok(serde_json::to_value(&self.state).unwrap_or_default())
        } else {
            self.last_custom_effects.clear();
            self.last_scheduled_actions.clear();
            Err(result.error.unwrap_or_else(|| "Unknown error".to_string()))
        }
    }

    fn current_status(&self) -> String {
        self.state.status.clone()
    }

    fn current_item_count(&self) -> usize {
        self.state.item_count
    }

    fn event_count(&self) -> usize {
        self.state.total_event_count
    }

    fn valid_actions(&self) -> Vec<String> {
        let ctx = self.eval_context();
        self.table
            .rules
            .iter()
            .filter(|rule| {
                let state_ok = rule.from_states.is_empty()
                    || rule.from_states.iter().any(|s| s == &self.state.status);
                if !state_ok {
                    return false;
                }
                rule.guard.check(&self.state.status, &ctx)
            })
            .map(|rule| rule.name.clone())
            .collect()
    }

    fn events_json(&self) -> serde_json::Value {
        serde_json::to_value(&self.state.events).unwrap_or(serde_json::Value::Array(vec![]))
    }

    fn spec_invariants(&self) -> &[SpecInvariant] {
        &self.invariants
    }

    fn bool_field(&self, var: &str) -> Option<bool> {
        self.state.booleans.get(var).copied()
    }

    fn pending_callbacks(&self) -> Vec<String> {
        self.last_custom_effects.clone()
    }
}

#[cfg(test)]
#[path = "sim_handler_test.rs"]
mod tests;
