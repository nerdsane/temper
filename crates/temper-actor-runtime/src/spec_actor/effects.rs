//! Effect vocabulary for the Postgres actor runtime (ARN-179).
//!
//! `validate_effect_support` decides at construction which
//! [`temper_jit::table::Effect`] variants this runtime executes;
//! `SpecDrivenActor::apply_effect` executes them. Both match exhaustively,
//! so a new `Effect` variant fails compilation here instead of being
//! silently dropped.

use std::collections::HashMap;

use temper_jit::table::TransitionTable;

use crate::actor::{ActorContext, ActorError, ActorHandle};

use super::{SpecActorState, SpecDrivenActor, SpecMessage};

// ─── Effect vocabulary support ───────────────────────────────────────────────

/// Validate that every effect in the compiled table is executable by this
/// runtime. This is the single source of truth for the Postgres actor
/// runtime's effect vocabulary (ARN-179): specs are rejected here, at
/// construction, instead of having unsupported effects silently dropped
/// during effect application.
///
/// The match is exhaustive on purpose — a new [`temper_jit::table::Effect`]
/// variant fails compilation here, forcing an explicit support decision.
pub fn validate_effect_support(
    table: &TransitionTable,
    routing: &HashMap<String, (String, String)>,
) -> Result<(), String> {
    use temper_jit::table::Effect;

    for rule in &table.rules {
        for effect in &rule.effects {
            match effect {
                Effect::SetState(_)
                | Effect::IncrementItems
                | Effect::DecrementItems
                | Effect::IncrementCounter(_)
                | Effect::IncrementCounterByParam { .. }
                | Effect::DecrementCounter(_)
                | Effect::DecrementCounterByParam { .. }
                | Effect::SetCounterFromParam { .. }
                | Effect::SetBool { .. }
                | Effect::ListAppend(_)
                | Effect::ListRemoveAt(_)
                // Emits are synthesized for every action; one without a
                // reaction rule simply has no listener.
                | Effect::EmitEvent(_) => {}
                Effect::Custom(trigger) => {
                    if !routing.contains_key(trigger.as_str()) {
                        return Err(format!(
                            "action {:?} uses trigger effect {trigger:?} with no reaction \
                             routing; wire a reaction rule for it or remove the trigger",
                            rule.name
                        ));
                    }
                }
                Effect::ScheduleAction { .. } => {
                    return Err(unsupported_effect(&rule.name, "schedule"));
                }
                Effect::ScheduleAtAction { .. } => {
                    return Err(unsupported_effect(&rule.name, "schedule_at"));
                }
                Effect::SpawnEntity { .. } => {
                    return Err(unsupported_effect(&rule.name, "spawn"));
                }
            }
        }
    }
    Ok(())
}

fn unsupported_effect(action: &str, effect_type: &str) -> String {
    format!(
        "action {action:?} uses effect type {effect_type:?}, which the Postgres actor \
         runtime cannot execute (it has no delayed delivery or per-entity spawning)"
    )
}

impl SpecDrivenActor {
    /// Apply one transition effect to actor state.
    ///
    /// The match is exhaustive — no catch-all — so every
    /// [`temper_jit::table::Effect`] variant has an explicit outcome and a
    /// new variant fails compilation instead of being silently dropped
    /// (ARN-179). Param-driven semantics mirror the canonical executor in
    /// `temper-server::entity_actor::effects::apply_effects`.
    pub(super) async fn apply_effect(
        &self,
        state: &mut SpecActorState,
        effect: &temper_jit::table::Effect,
        params: &serde_json::Value,
        ctx: &ActorContext,
    ) -> Result<(), ActorError> {
        match effect {
            temper_jit::table::Effect::SetState(s) => {
                state.status = s.clone();
            }
            temper_jit::table::Effect::IncrementItems => {
                *state.counters.entry("items".into()).or_default() += 1;
            }
            temper_jit::table::Effect::IncrementCounter(var) => {
                *state.counters.entry(var.clone()).or_default() += 1;
            }
            temper_jit::table::Effect::IncrementCounterByParam { var, param } => {
                let delta = counter_delta_from_params(params, param);
                *state.counters.entry(var.clone()).or_default() += delta;
            }
            temper_jit::table::Effect::DecrementItems => {
                let c = state.counters.entry("items".into()).or_default();
                *c = c.saturating_sub(1);
            }
            temper_jit::table::Effect::DecrementCounter(var) => {
                let c = state.counters.entry(var.clone()).or_default();
                *c = c.saturating_sub(1);
            }
            temper_jit::table::Effect::DecrementCounterByParam { var, param } => {
                let delta = counter_delta_from_params(params, param);
                let c = state.counters.entry(var.clone()).or_default();
                *c = c.saturating_sub(delta);
            }
            temper_jit::table::Effect::SetCounterFromParam { var, param } => {
                let parsed = params
                    .get(param)
                    .and_then(|v| {
                        v.as_u64()
                            .or_else(|| v.as_i64().and_then(|n| u64::try_from(n).ok()))
                    })
                    .and_then(|n| usize::try_from(n).ok());
                match parsed {
                    Some(value) => {
                        state.counters.insert(var.clone(), value);
                    }
                    None => tracing::warn!(
                        actor = %self.name,
                        counter = %var,
                        param = %param,
                        "set_counter_from_param skipped because param was missing or not a non-negative integer"
                    ),
                }
            }
            temper_jit::table::Effect::SetBool { var, value } => {
                state.booleans.insert(var.clone(), *value);
            }
            temper_jit::table::Effect::ListAppend(var) => {
                if let Some(val) = params.get(var).and_then(|v| v.as_str()) {
                    state
                        .lists
                        .entry(var.clone())
                        .or_default()
                        .push(val.to_string());
                }
            }
            temper_jit::table::Effect::ListRemoveAt(var) => {
                let index_key = format!("{var}_index");
                if let Some(idx) = params.get(&index_key).and_then(|v| v.as_u64()) {
                    let list = state.lists.entry(var.clone()).or_default();
                    let idx = idx as usize;
                    if idx < list.len() {
                        list.remove(idx);
                    }
                }
            }
            temper_jit::table::Effect::EmitEvent(emit_name) => {
                if let Some((target_type, target_action)) = self.routing.get(emit_name.as_str()) {
                    tracing::info!(actor=%self.name, emit=%emit_name, target=%target_type, target_action=%target_action, "routing emit");
                    let target =
                        ActorHandle::new(ctx.self_handle().namespace.clone(), target_type.clone());
                    ctx.tell(
                        &target,
                        SpecMessage::with_params(target_action.clone(), state.fields.clone()),
                    )
                    .await;
                } else {
                    tracing::warn!(
                        actor = %self.name,
                        emit = %emit_name,
                        "no routing for emit (no reaction rule)"
                    );
                }
            }
            temper_jit::table::Effect::Custom(trigger_name) => {
                let Some((target_type, target_action)) = self.routing.get(trigger_name.as_str())
                else {
                    // Unreachable: construction rejects unrouted triggers.
                    return Err(ActorError::HandlerFailed(format!(
                        "trigger {trigger_name:?} has no reaction routing"
                    )));
                };
                tracing::info!(actor=%self.name, trigger=%trigger_name, target=%target_type, target_action=%target_action, "routing trigger");
                let target =
                    ActorHandle::new(ctx.self_handle().namespace.clone(), target_type.clone());
                ctx.tell(
                    &target,
                    SpecMessage::with_params(target_action.clone(), state.fields.clone()),
                )
                .await;
            }
            // Unreachable: construction rejects these via validate_effect_support.
            // Failing loudly here keeps a table that bypassed construction from
            // silently dropping effects.
            temper_jit::table::Effect::ScheduleAction { .. }
            | temper_jit::table::Effect::ScheduleAtAction { .. }
            | temper_jit::table::Effect::SpawnEntity { .. } => {
                return Err(ActorError::HandlerFailed(format!(
                    "effect {effect:?} is not executable by the Postgres actor runtime"
                )));
            }
        }
        Ok(())
    }
}

/// Numeric delta for the by-param counter effects. Mirrors
/// `temper-server::entity_actor::effects::counter_delta_from_params`:
/// accepts a non-negative number or numeric string, defaults to 0.
fn counter_delta_from_params(params: &serde_json::Value, param: &str) -> usize {
    params
        .get(param)
        .and_then(|value| match value {
            serde_json::Value::Number(number) => number.as_u64().map(|v| v as usize),
            serde_json::Value::String(text) => text.parse::<usize>().ok(),
            _ => None,
        })
        .unwrap_or(0)
}
