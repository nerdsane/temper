//! Core types for transition tables.
//!
//! A [`TransitionTable`] encodes the complete set of transition rules for a single
//! entity type as DATA, not code. It can be hot-swapped per-actor without restart.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Core types
// ---------------------------------------------------------------------------

/// A transition table: state machine transitions as DATA, not code.
/// Can be hot-swapped per-actor without restart.
#[derive(Debug, Clone, Serialize)]
pub struct TransitionTable {
    /// The entity this table governs (e.g. "Order").
    pub entity_name: String,
    /// All valid state values.
    pub states: Vec<String>,
    /// The state an entity starts in.
    pub initial_state: String,
    /// Ordered list of transition rules.
    pub rules: Vec<TransitionRule>,
    /// Pre-built index: action name → indices into `rules`.
    ///
    /// Eliminates the O(N) linear scan + Vec allocation in [`evaluate_ctx()`].
    /// Rebuilt automatically during construction and deserialization.
    #[serde(skip)]
    pub(crate) rule_index: BTreeMap<String, Vec<usize>>,
}

impl<'de> Deserialize<'de> for TransitionTable {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        /// Helper struct for deserializing the persistent fields only.
        #[derive(Deserialize)]
        struct TransitionTableRaw {
            entity_name: String,
            states: Vec<String>,
            initial_state: String,
            rules: Vec<TransitionRule>,
        }

        let raw = TransitionTableRaw::deserialize(deserializer)?;
        let mut table = TransitionTable {
            entity_name: raw.entity_name,
            states: raw.states,
            initial_state: raw.initial_state,
            rules: raw.rules,
            rule_index: BTreeMap::new(),
        };
        table.rebuild_index();
        Ok(table)
    }
}

/// A single transition rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransitionRule {
    /// Action name (e.g. "SubmitOrder").
    pub name: String,
    /// States this transition may fire from.
    pub from_states: Vec<String>,
    /// Target state after the transition (if deterministic).
    pub to_state: Option<String>,
    /// Guard condition evaluated before the transition fires.
    pub guard: Guard,
    /// Effects applied after the transition fires.
    pub effects: Vec<Effect>,
}

/// A guard condition (evaluated before a transition fires).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Guard {
    /// No guard -- always passes.
    Always,
    /// Current state must be in the given set.
    StateIn(Vec<String>),
    /// `items.len() >= N` (legacy alias for `CounterMin { var: "items", min: N }`).
    ItemCountMin(usize),
    /// A named counter must be >= N.
    CounterMin { var: String, min: usize },
    /// A named counter must be < N.
    CounterMax { var: String, max: usize },
    /// A named boolean variable must be true.
    BoolTrue(String),
    /// A named list variable must contain a specific value.
    ListContains { var: String, value: String },
    /// A named list variable must have at least N elements.
    ListLengthMin { var: String, min: usize },
    /// Another entity must be in one of the required statuses (pre-resolved as boolean).
    CrossEntityStateIn {
        entity_type: String,
        entity_id_source: String,
        required_status: Vec<String>,
    },
    /// All inner guards must pass.
    And(Vec<Guard>),
}

/// An effect applied after a transition fires.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Effect {
    /// Change the entity status.
    SetState(String),
    /// Add an item (legacy alias for `IncrementCounter("items")`).
    IncrementItems,
    /// Remove an item (legacy alias for `DecrementCounter("items")`).
    DecrementItems,
    /// Increment a named counter variable.
    IncrementCounter(String),
    /// Decrement a named counter variable.
    DecrementCounter(String),
    /// Set a named boolean variable.
    SetBool { var: String, value: bool },
    /// Emit a named event.
    EmitEvent(String),
    /// Append a value to a named list variable (value from action params).
    ListAppend(String),
    /// Remove a value from a named list variable by index (index from action params).
    ListRemoveAt(String),
    /// Domain-specific custom effect (e.g., "DeploySpecs", "NotifyAdmin").
    ///
    /// Dispatched by post-transition hooks registered at startup.
    /// The actor runtime ignores unknown custom effects — they are only
    /// meaningful to the hook that registered for them.
    Custom(String),
    /// Schedule a delayed action (timer fires after delay_seconds).
    ScheduleAction { action: String, delay_seconds: u64 },
    /// Spawn a child entity as a post-transition effect.
    SpawnEntity {
        entity_type: String,
        entity_id_source: String,
        initial_action: Option<String>,
        store_id_in: Option<String>,
    },
}

/// The result of evaluating a transition.
#[derive(Debug, Clone, PartialEq)]
pub struct TransitionResult {
    /// The new state after the transition (may be unchanged).
    pub new_state: String,
    /// Effects that were applied.
    pub effects: Vec<Effect>,
    /// Whether the transition succeeded.
    pub success: bool,
}

/// Runtime context for guard evaluation.
///
/// Passed to [`Guard::check()`] to provide the full entity state. The legacy
/// [`Guard::evaluate()`] method builds a minimal context from `item_count`.
#[derive(Debug, Clone, Default)]
pub struct EvalContext {
    /// Named counter values (e.g., "items" -> 2, "review_cycles" -> 1).
    pub counters: BTreeMap<String, usize>,
    /// Named boolean values (e.g., "assignee_set" -> true).
    pub booleans: BTreeMap<String, bool>,
    /// Named list values (e.g., "tags" -> ["urgent", "review"]).
    pub lists: BTreeMap<String, Vec<String>>,
}

impl TransitionTable {
    /// Rebuild the rule index from the current rules vec.
    ///
    /// Called automatically during construction. Must be called explicitly
    /// after deserialization (since `rule_index` is `#[serde(skip)]`).
    pub fn rebuild_index(&mut self) {
        self.rule_index.clear();
        for (i, rule) in self.rules.iter().enumerate() {
            self.rule_index
                .entry(rule.name.clone())
                .or_default()
                .push(i);
        }
    }
}

// ---------------------------------------------------------------------------
// Guard evaluation
// ---------------------------------------------------------------------------

impl Guard {
    /// Evaluate this guard against the current runtime context (legacy API).
    ///
    /// Uses a single `item_count` mapped to the `"items"` counter. For
    /// multi-counter or boolean guard support, use [`check()`](Self::check).
    pub fn evaluate(&self, current_state: &str, item_count: usize) -> bool {
        let mut ctx = EvalContext::default();
        ctx.counters.insert("items".to_string(), item_count);
        self.check(current_state, &ctx)
    }

    /// Evaluate this guard against a full evaluation context.
    pub fn check(&self, current_state: &str, ctx: &EvalContext) -> bool {
        match self {
            Guard::Always => true,
            Guard::StateIn(states) => states.iter().any(|s| s == current_state),
            Guard::ItemCountMin(n) => ctx.counters.get("items").copied().unwrap_or(0) >= *n,
            Guard::CounterMin { var, min } => ctx.counters.get(var).copied().unwrap_or(0) >= *min,
            Guard::CounterMax { var, max } => ctx.counters.get(var).copied().unwrap_or(0) < *max,
            Guard::BoolTrue(var) => ctx.booleans.get(var).copied().unwrap_or(false),
            Guard::ListContains { var, value } => {
                ctx.lists.get(var).is_some_and(|list| list.contains(value))
            }
            Guard::ListLengthMin { var, min } => {
                ctx.lists.get(var).map_or(0, |list| list.len()) >= *min
            }
            Guard::CrossEntityStateIn {
                entity_type,
                entity_id_source,
                ..
            } => {
                let key = format!("__xref:{}:{}", entity_type, entity_id_source);
                ctx.booleans.get(&key).copied().unwrap_or(false)
            }
            Guard::And(guards) => guards.iter().all(|g| g.check(current_state, ctx)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn guard_always_passes() {
        let guard = Guard::Always;
        let ctx = EvalContext::default();
        assert!(guard.check("Draft", &ctx));
    }

    #[test]
    fn guard_state_in_matches() {
        let guard = Guard::StateIn(vec!["Draft".to_string(), "Active".to_string()]);
        let ctx = EvalContext::default();
        assert!(guard.check("Draft", &ctx));
    }

    #[test]
    fn guard_state_in_no_match() {
        let guard = Guard::StateIn(vec!["Draft".to_string()]);
        let ctx = EvalContext::default();
        assert!(!guard.check("Active", &ctx));
    }

    #[test]
    fn guard_item_count_min_passes() {
        let guard = Guard::ItemCountMin(2);
        let mut ctx = EvalContext::default();
        ctx.counters.insert("items".to_string(), 3);
        assert!(guard.check("Draft", &ctx));
    }

    #[test]
    fn guard_item_count_min_fails() {
        let guard = Guard::ItemCountMin(2);
        let mut ctx = EvalContext::default();
        ctx.counters.insert("items".to_string(), 1);
        assert!(!guard.check("Draft", &ctx));
    }

    #[test]
    fn guard_counter_min_passes() {
        let guard = Guard::CounterMin {
            var: "cycles".to_string(),
            min: 2,
        };
        let mut ctx = EvalContext::default();
        ctx.counters.insert("cycles".to_string(), 3);
        assert!(guard.check("Draft", &ctx));
    }

    #[test]
    fn guard_counter_max_passes() {
        let guard = Guard::CounterMax {
            var: "retries".to_string(),
            max: 3,
        };
        let mut ctx = EvalContext::default();
        ctx.counters.insert("retries".to_string(), 2);
        assert!(guard.check("Draft", &ctx));
    }

    #[test]
    fn guard_counter_max_fails() {
        let guard = Guard::CounterMax {
            var: "retries".to_string(),
            max: 3,
        };
        let mut ctx = EvalContext::default();
        ctx.counters.insert("retries".to_string(), 3);
        assert!(!guard.check("Draft", &ctx));
    }

    #[test]
    fn guard_bool_true_passes() {
        let guard = Guard::BoolTrue("assigned".to_string());
        let mut ctx = EvalContext::default();
        ctx.booleans.insert("assigned".to_string(), true);
        assert!(guard.check("Draft", &ctx));
    }

    #[test]
    fn guard_bool_true_fails_missing() {
        let guard = Guard::BoolTrue("assigned".to_string());
        let ctx = EvalContext::default();
        assert!(!guard.check("Draft", &ctx));
    }

    #[test]
    fn guard_and_all_pass() {
        let guard = Guard::And(vec![
            Guard::Always,
            Guard::StateIn(vec!["Draft".to_string()]),
        ]);
        let ctx = EvalContext::default();
        assert!(guard.check("Draft", &ctx));
    }

    #[test]
    fn guard_and_one_fails() {
        let guard = Guard::And(vec![
            Guard::Always,
            Guard::StateIn(vec!["Active".to_string()]),
        ]);
        let ctx = EvalContext::default();
        assert!(!guard.check("Draft", &ctx));
    }

    #[test]
    fn rebuild_index_groups_by_name() {
        let mut table = TransitionTable {
            entity_name: "TestEntity".to_string(),
            states: vec!["Draft".to_string(), "Active".to_string()],
            initial_state: "Draft".to_string(),
            rules: vec![
                TransitionRule {
                    name: "Submit".to_string(),
                    from_states: vec!["Draft".to_string()],
                    to_state: Some("Active".to_string()),
                    guard: Guard::Always,
                    effects: vec![],
                },
                TransitionRule {
                    name: "Submit".to_string(),
                    from_states: vec!["Active".to_string()],
                    to_state: Some("Draft".to_string()),
                    guard: Guard::Always,
                    effects: vec![],
                },
                TransitionRule {
                    name: "Cancel".to_string(),
                    from_states: vec!["Draft".to_string()],
                    to_state: Some("Draft".to_string()),
                    guard: Guard::Always,
                    effects: vec![],
                },
            ],
            rule_index: BTreeMap::new(),
        };
        table.rebuild_index();

        assert_eq!(table.rule_index.len(), 2);
        assert_eq!(table.rule_index["Submit"], vec![0, 1]);
        assert_eq!(table.rule_index["Cancel"], vec![2]);
    }
}
