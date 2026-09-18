//! Actor-side binding and freshness validation for external judgments.

use std::collections::BTreeMap;

use serde_json::Value;
use sha2::{Digest, Sha256};
use temper_jit::table::{Guard, TransitionTable};
use temper_spec::automaton::SystemOneGuard;

use crate::entity_actor::EntityState;

/// Runtime-supplied evidence bound to one action attempt and exact pre-state.
///
/// This is an actor message payload, never an HTTP action parameter. Receipts
/// are persisted by dispatch before constructing this payload. Construction is
/// restricted to the kernel; application callers cannot fabricate trusted answers.
#[derive(Debug, Clone)]
pub struct SystemOneEvidence {
    /// Durable evaluation streams supporting this transition.
    pub(crate) receipt_ids: Vec<String>,
    /// Exact state that was used to resolve the evaluation inputs.
    pub(crate) expected_state: String,
    /// Complete transition table identity used by dispatch.
    pub(crate) expected_table: String,
    /// Identity of the logical action attempt.
    pub(crate) attempt_id: String,
    /// Digest of the validated action parameters.
    pub(crate) params_digest: String,
    /// Validated assertion outcomes indexed by compiled guard identity.
    pub(crate) outcomes: BTreeMap<String, bool>,
}

impl SystemOneEvidence {
    /// Check freshness and return the trusted guard outcomes for this action.
    pub(crate) fn validate(
        &self,
        table: &TransitionTable,
        state: &EntityState,
        action: &str,
        params: &Value,
        attempt_id: Option<&str>,
    ) -> Result<(), String> {
        if self.expected_state
            != crate::entity_actor::effects::entity_authorization_precondition(state)
            || self.expected_table != table_digest(table)?
        {
            return Err("system_one evidence became stale; use a new action attempt".into());
        }
        if attempt_id != Some(self.attempt_id.as_str())
            || self.params_digest != json_digest(params)?
        {
            return Err("system_one evidence does not match the action attempt".into());
        }
        let guards = collect_guards(table, action);
        if self.receipt_ids.len() != guards.len()
            || guards.iter().any(|g| !self.outcomes.contains_key(&g.key()))
        {
            return Err("system_one evidence is incomplete".into());
        }
        Ok(())
    }
}

pub(crate) fn json_digest(value: &Value) -> Result<String, String> {
    fn canonical(value: &Value) -> Value {
        match value {
            Value::Object(values) => {
                let sorted: BTreeMap<_, _> = values
                    .iter()
                    .map(|(key, value)| (key.clone(), canonical(value)))
                    .collect();
                Value::Object(sorted.into_iter().collect())
            }
            Value::Array(values) => Value::Array(values.iter().map(canonical).collect()),
            _ => value.clone(),
        }
    }
    serde_json::to_vec(&canonical(value))
        .map(|bytes| format!("{:x}", Sha256::digest(bytes)))
        .map_err(|_| "cannot serialize system_one binding".into())
}

pub(crate) fn table_digest(table: &TransitionTable) -> Result<String, String> {
    let value = serde_json::to_value(table).map_err(|_| "cannot serialize transition table")?;
    json_digest(&value)
}

pub(crate) fn collect_guards<'a>(
    table: &'a TransitionTable,
    action: &str,
) -> Vec<&'a SystemOneGuard> {
    fn visit<'a>(guard: &'a Guard, found: &mut Vec<&'a SystemOneGuard>) {
        match guard {
            Guard::SystemOne(guard) => found.push(guard),
            Guard::And(guards) => guards.iter().for_each(|g| visit(g, found)),
            _ => {}
        }
    }
    let mut guards = Vec::new();
    for rule in &table.rules {
        if rule.name == action {
            visit(&rule.guard, &mut guards);
        }
    }
    guards
}
