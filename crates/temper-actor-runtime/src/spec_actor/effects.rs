//! Effect application for SpecDrivenActor (ADR-0168 / ARN-179).

use temper_jit::table::TransitionTable;

use super::{SpecActorState, SpecDrivenActor, SpecMessage};
use crate::actor::{ActorContext, ActorHandle};

/// Reject schedule/spawn effects this backend cannot execute (ADR-0168).
pub(crate) fn reject_unsupported_effects(table: &TransitionTable) -> Result<(), String> {
    use temper_jit::table::Effect;
    for rule in &table.rules {
        for effect in &rule.effects {
            match effect {
                Effect::ScheduleAction { .. } => {
                    return Err(format!(
                        "unsupported effect 'schedule' on action '{}': \
                         PG actor-runtime has no timer pipeline (ADR-0168 / ARN-179)",
                        rule.name
                    ));
                }
                Effect::ScheduleAtAction { .. } => {
                    return Err(format!(
                        "unsupported effect 'schedule_at' on action '{}': \
                         PG actor-runtime has no timer pipeline (ADR-0168 / ARN-179)",
                        rule.name
                    ));
                }
                Effect::SpawnEntity { .. } => {
                    return Err(format!(
                        "unsupported effect 'spawn' on action '{}': \
                         PG actor-runtime has no spawn pipeline (ADR-0168 / ARN-179)",
                        rule.name
                    ));
                }
                // All other effects are either implemented or legacy no-ops.
                Effect::SetState(_)
                | Effect::IncrementItems
                | Effect::DecrementItems
                | Effect::IncrementCounter(_)
                | Effect::IncrementCounterByParam { .. }
                | Effect::DecrementCounter(_)
                | Effect::DecrementCounterByParam { .. }
                | Effect::SetCounterFromParam { .. }
                | Effect::SetBool { .. }
                | Effect::EmitEvent(_)
                | Effect::ListAppend(_)
                | Effect::ListRemoveAt(_)
                | Effect::Custom(_) => {}
            }
        }
    }
    Ok(())
}

fn counter_delta_from_params(params: &serde_json::Value, param: &str) -> Option<usize> {
    params.get(param).and_then(|value| match value {
        serde_json::Value::Number(number) => number.as_u64().map(|v| v as usize),
        serde_json::Value::String(text) => text.parse::<usize>().ok(),
        _ => None,
    })
}

impl SpecDrivenActor {
    /// Apply a compiled JIT effect to durable actor state (ADR-0168).
    ///
    /// Action params live in `state.fields` after the handle() merge step.
    /// Exhaustive match — no silent catch-all.
    pub(crate) async fn apply_effect_inner(
        &self,
        state: &mut SpecActorState,
        effect: &temper_jit::table::Effect,
        ctx: &ActorContext,
    ) {
        use temper_jit::table::Effect;
        match effect {
            Effect::SetState(s) => {
                state.status = s.clone();
            }
            Effect::IncrementItems => {
                *state.counters.entry("items".into()).or_default() += 1;
            }
            Effect::IncrementCounter(var) => {
                *state.counters.entry(var.clone()).or_default() += 1;
            }
            Effect::IncrementCounterByParam { var, param } => {
                match counter_delta_from_params(&state.fields, param) {
                    Some(delta) => *state.counters.entry(var.clone()).or_default() += delta,
                    None => tracing::warn!(
                        actor = %self.name,
                        counter = %var,
                        param = %param,
                        "increment_counter_by_param skipped: param missing or not a non-negative integer"
                    ),
                }
            }
            Effect::DecrementItems => {
                let c = state.counters.entry("items".into()).or_default();
                *c = c.saturating_sub(1);
            }
            Effect::DecrementCounter(var) => {
                let c = state.counters.entry(var.clone()).or_default();
                *c = c.saturating_sub(1);
            }
            Effect::DecrementCounterByParam { var, param } => {
                match counter_delta_from_params(&state.fields, param) {
                    Some(delta) => {
                        let c = state.counters.entry(var.clone()).or_default();
                        *c = c.saturating_sub(delta);
                    }
                    None => tracing::warn!(
                        actor = %self.name,
                        counter = %var,
                        param = %param,
                        "decrement_counter_by_param skipped: param missing or not a non-negative integer"
                    ),
                }
            }
            Effect::SetCounterFromParam { var, param } => {
                let parsed = state.fields.get(param).and_then(|v| match v {
                    serde_json::Value::Number(n) => n.as_u64().map(|u| u as usize),
                    serde_json::Value::String(t) => t.parse::<usize>().ok(),
                    _ => None,
                });
                match parsed {
                    Some(value) => {
                        state.counters.insert(var.clone(), value);
                    }
                    None => tracing::warn!(
                        actor = %self.name,
                        counter = %var,
                        param = %param,
                        "set_counter_from_param skipped: param missing or not a non-negative integer"
                    ),
                }
            }
            Effect::SetBool { var, value } => {
                state.booleans.insert(var.clone(), *value);
            }
            Effect::ListAppend(var) => {
                // Same semantics as entity_actor: value is params[var] as string.
                if let Some(val) = state.fields.get(var).and_then(|v| v.as_str()) {
                    state
                        .lists
                        .entry(var.clone())
                        .or_default()
                        .push(val.to_string());
                } else {
                    tracing::warn!(
                        actor = %self.name,
                        list = %var,
                        "list_append skipped: param missing or not a string"
                    );
                }
            }
            Effect::ListRemoveAt(var) => {
                let index_key = format!("{var}_index");
                if let Some(idx) = state.fields.get(&index_key).and_then(|v| v.as_u64()) {
                    let list = state.lists.entry(var.clone()).or_default();
                    let idx = idx as usize;
                    if idx < list.len() {
                        list.remove(idx);
                    }
                } else {
                    tracing::warn!(
                        actor = %self.name,
                        list = %var,
                        index_key = %index_key,
                        "list_remove_at skipped: index param missing"
                    );
                }
            }
            Effect::EmitEvent(emit_name) => {
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
            Effect::Custom(trigger_name) => {
                if let Some((target_type, target_action)) = self.routing.get(trigger_name.as_str())
                {
                    tracing::info!(actor=%self.name, trigger=%trigger_name, target=%target_type, target_action=%target_action, "routing trigger");
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
                        trigger = %trigger_name,
                        "no routing for trigger"
                    );
                }
            }
            // Construction rejects these (reject_unsupported_effects).
            // Fail fast if a table somehow bypasses the constructor (ADR-0168).
            Effect::ScheduleAction { .. }
            | Effect::ScheduleAtAction { .. }
            | Effect::SpawnEntity { .. } => {
                unreachable!(
                    "schedule/spawn effect reached apply_effect after construction rejection \
                     (ADR-0168 / ARN-179); actor={}",
                    self.name
                );
            }
        }
    }
}
