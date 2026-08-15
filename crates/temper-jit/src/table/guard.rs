//! Guard conditions and their evaluation.
//!
//! A [`Guard`] is the precondition on a [`super::types::TransitionRule`]. It is
//! evaluated against an [`EvalContext`] before a transition fires. Two
//! evaluators are provided:
//!
//! - [`Guard::check`] — the bare-bool fast path on the hot dispatch route.
//! - [`Guard::check_detailed`] — the cold rejection path, returning a
//!   [`GuardFailure`] that names the first failing sub-guard so the server can
//!   render an agent-facing self-heal error (ADR-0151).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

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
    /// A named boolean variable must be false.
    BoolFalse(String),
    /// A named list variable must contain a specific value.
    ListContains { var: String, value: String },
    /// A named list variable must have at least N elements.
    ListLengthMin { var: String, min: usize },
    /// A cross-entity status precondition on a related entity (pre-resolved as
    /// a boolean by the runtime resolver).
    CrossEntityStateIn {
        /// The target entity type (e.g. "File").
        entity_type: String,
        /// Field on the current entity holding the target entity ID.
        entity_id_source: String,
        /// Allowlist: target must be in one of these statuses (any match
        /// passes). Empty ⇒ no allowlist constraint.
        required_status: Vec<String>,
        /// Denylist: target must NOT be in any of these statuses. Empty ⇒ no
        /// denylist constraint. Lets a guard reject only when the container is
        /// in a specific bad state (e.g. Workspace `Frozen`/`Archived`) while a
        /// missing/unresolved target still passes.
        #[serde(default)]
        forbidden_status: Vec<String>,
        /// Whether the ref must be present (ARN-92 #2). Threaded to the
        /// runtime cross-entity resolver, which treats an empty *required*
        /// scalar/list ref as a failing guard rather than a vacuous pass. Guard
        /// evaluation here is unaffected — the resolver pre-computes the
        /// boolean this guard reads.
        #[serde(default)]
        required: bool,
    },
    /// An incoming typed reference must equal a stored typed reference.
    ReferenceEquals { reference: String, param: String },
    /// All inner guards must pass.
    And(Vec<Guard>),
}

/// The variant of guard that failed, carried in a [`GuardFailure`].
///
/// Mirrors the [`Guard`] variants that can be rejected. `Always` never appears
/// because it cannot fail.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum GuardFailureKind {
    /// A [`Guard::StateIn`] did not include the current state.
    StateIn,
    /// A [`Guard::ItemCountMin`] floor was not met.
    ItemCountMin,
    /// A [`Guard::CounterMin`] floor was not met.
    CounterMin,
    /// A [`Guard::CounterMax`] ceiling was reached.
    CounterMax,
    /// A [`Guard::BoolTrue`] boolean was not true.
    BoolTrue,
    /// A [`Guard::BoolFalse`] boolean was not false.
    BoolFalse,
    /// A [`Guard::ListContains`] list did not contain the required value.
    ListContains,
    /// A [`Guard::ListLengthMin`] list was too short.
    ListLengthMin,
    /// A [`Guard::CrossEntityStateIn`] cross-entity status check failed.
    CrossEntityState,
    /// A [`Guard::ReferenceEquals`] identity comparison failed.
    ReferenceEquals,
}

impl GuardFailureKind {
    /// The stable, agent-facing label for this kind. Matches the IOA guard
    /// `type` keyword (e.g. `min_count`, `is_true`, `cross_entity_state`) so the
    /// failure names a guard the author can find in the spec grammar.
    pub fn label(&self) -> &'static str {
        match self {
            GuardFailureKind::StateIn => "state_in",
            // `items` count and named-counter floors both surface as `min_count`
            // (the IOA keyword); the bare `items` infix form lowers to the same.
            GuardFailureKind::ItemCountMin => "min_count",
            GuardFailureKind::CounterMin => "min_count",
            GuardFailureKind::CounterMax => "max_count",
            GuardFailureKind::BoolTrue => "is_true",
            GuardFailureKind::BoolFalse => "is_false",
            GuardFailureKind::ListContains => "list_contains",
            GuardFailureKind::ListLengthMin => "list_length_min",
            GuardFailureKind::CrossEntityState => "cross_entity_state",
            GuardFailureKind::ReferenceEquals => "reference_equals",
        }
    }
}

/// Identity of the first failing sub-guard on a rejected transition (ADR-0151).
///
/// Produced by [`Guard::check_detailed`] and carried on
/// [`super::types::TransitionResult::guard_failure`]. The server renders it into
/// a specific error string that names which precondition failed, on which
/// field/ref, and the required-vs-found values where the guard exposes them.
///
/// Derives `Serialize`/`Deserialize` via `serde` only — no `temper-verify`
/// dependency is introduced.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GuardFailure {
    /// Which guard variant failed.
    pub kind: GuardFailureKind,
    /// The field/counter/bool/list/ref the guard read, when the variant names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub var: Option<String>,
    /// The required value/constraint, rendered for the agent, where available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<String>,
    /// The observed value, rendered for the agent, where available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub found: Option<String>,
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
    /// Stored scalar string fields.
    pub strings: BTreeMap<String, String>,
    /// Normalized incoming scalar string parameters.
    pub params: BTreeMap<String, String>,
}

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

    /// Evaluate this guard against a full evaluation context (bare-bool fast path).
    pub fn check(&self, current_state: &str, ctx: &EvalContext) -> bool {
        match self {
            Guard::Always => true,
            Guard::StateIn(states) => states.iter().any(|s| s == current_state),
            Guard::ItemCountMin(n) => ctx.counters.get("items").copied().unwrap_or(0) >= *n,
            Guard::CounterMin { var, min } => ctx.counters.get(var).copied().unwrap_or(0) >= *min,
            Guard::CounterMax { var, max } => ctx.counters.get(var).copied().unwrap_or(0) < *max,
            Guard::BoolTrue(var) => ctx.booleans.get(var).copied().unwrap_or(false),
            Guard::BoolFalse(var) => !ctx.booleans.get(var).copied().unwrap_or(false),
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
            Guard::ReferenceEquals { reference, param } => {
                matches!(
                    (ctx.strings.get(reference), ctx.params.get(param)),
                    (Some(stored), Some(incoming))
                        if !stored.is_empty() && !incoming.is_empty() && stored == incoming
                )
            }
            Guard::And(guards) => guards.iter().all(|g| g.check(current_state, ctx)),
        }
    }

    /// Evaluate this guard, naming the first failing sub-guard on rejection.
    ///
    /// Returns `None` when the guard holds. On failure, returns a
    /// [`GuardFailure`] identifying the offending variant, the field/ref it
    /// read, and the required-vs-found values where the variant exposes them.
    ///
    /// `And` recurses in source order and short-circuits on the first failing
    /// conjunct, matching the evaluation order of [`check`](Self::check). This
    /// is the cold rejection path; the hot path uses `check` and never
    /// allocates a [`GuardFailure`]. Both walk the same predicates, so they
    /// agree on whether the guard holds.
    pub fn check_detailed(&self, current_state: &str, ctx: &EvalContext) -> Option<GuardFailure> {
        match self {
            Guard::Always => None,
            Guard::StateIn(states) => {
                if states.iter().any(|s| s == current_state) {
                    None
                } else {
                    Some(GuardFailure {
                        kind: GuardFailureKind::StateIn,
                        var: None,
                        required: Some(format!("state in [{}]", states.join(","))),
                        found: Some(current_state.to_string()),
                    })
                }
            }
            Guard::ItemCountMin(n) => {
                let found = ctx.counters.get("items").copied().unwrap_or(0);
                if found >= *n {
                    None
                } else {
                    Some(GuardFailure {
                        kind: GuardFailureKind::ItemCountMin,
                        var: Some("items".to_string()),
                        required: Some(format!(">= {n}")),
                        found: Some(found.to_string()),
                    })
                }
            }
            Guard::CounterMin { var, min } => {
                let found = ctx.counters.get(var).copied().unwrap_or(0);
                if found >= *min {
                    None
                } else {
                    Some(GuardFailure {
                        kind: GuardFailureKind::CounterMin,
                        var: Some(var.clone()),
                        required: Some(format!(">= {min}")),
                        found: Some(found.to_string()),
                    })
                }
            }
            Guard::CounterMax { var, max } => {
                let found = ctx.counters.get(var).copied().unwrap_or(0);
                if found < *max {
                    None
                } else {
                    Some(GuardFailure {
                        kind: GuardFailureKind::CounterMax,
                        var: Some(var.clone()),
                        required: Some(format!("< {max}")),
                        found: Some(found.to_string()),
                    })
                }
            }
            Guard::BoolTrue(var) => {
                if ctx.booleans.get(var).copied().unwrap_or(false) {
                    None
                } else {
                    Some(GuardFailure {
                        kind: GuardFailureKind::BoolTrue,
                        var: Some(var.clone()),
                        required: Some("true".to_string()),
                        found: Some(found_bool(ctx, var)),
                    })
                }
            }
            Guard::BoolFalse(var) => {
                if !ctx.booleans.get(var).copied().unwrap_or(false) {
                    None
                } else {
                    Some(GuardFailure {
                        kind: GuardFailureKind::BoolFalse,
                        var: Some(var.clone()),
                        required: Some("false".to_string()),
                        found: Some(found_bool(ctx, var)),
                    })
                }
            }
            Guard::ListContains { var, value } => {
                if ctx.lists.get(var).is_some_and(|list| list.contains(value)) {
                    None
                } else {
                    Some(GuardFailure {
                        kind: GuardFailureKind::ListContains,
                        var: Some(var.clone()),
                        required: Some(format!("contains '{value}'")),
                        found: None,
                    })
                }
            }
            Guard::ListLengthMin { var, min } => {
                let found = ctx.lists.get(var).map_or(0, |list| list.len());
                if found >= *min {
                    None
                } else {
                    Some(GuardFailure {
                        kind: GuardFailureKind::ListLengthMin,
                        var: Some(var.clone()),
                        required: Some(format!("length >= {min}")),
                        found: Some(found.to_string()),
                    })
                }
            }
            Guard::CrossEntityStateIn {
                entity_type,
                entity_id_source,
                required_status,
                forbidden_status,
                ..
            } => {
                let key = format!("__xref:{}:{}", entity_type, entity_id_source);
                if ctx.booleans.get(&key).copied().unwrap_or(false) {
                    None
                } else {
                    let required = match (required_status.is_empty(), forbidden_status.is_empty()) {
                        (false, true) => {
                            format!("{entity_type} status in [{}]", required_status.join(","))
                        }
                        (true, false) => {
                            format!(
                                "{entity_type} status not in [{}]",
                                forbidden_status.join(",")
                            )
                        }
                        (false, false) => format!(
                            "{entity_type} status in [{}] and not in [{}]",
                            required_status.join(","),
                            forbidden_status.join(",")
                        ),
                        (true, true) => format!("{entity_type} status precondition"),
                    };
                    Some(GuardFailure {
                        kind: GuardFailureKind::CrossEntityState,
                        var: Some(entity_id_source.clone()),
                        required: Some(required),
                        // The runtime pre-resolves the cross-entity status into a
                        // boolean; the unresolved/missing target reads as false.
                        found: Some("<unsatisfied>".to_string()),
                    })
                }
            }
            Guard::ReferenceEquals { reference, param } => {
                let stored = ctx.strings.get(reference).filter(|value| !value.is_empty());
                let incoming = ctx.params.get(param).filter(|value| !value.is_empty());
                if stored == incoming && stored.is_some() {
                    None
                } else {
                    Some(GuardFailure {
                        kind: GuardFailureKind::ReferenceEquals,
                        var: Some(reference.clone()),
                        required: Some(format!(
                            "{reference}={}",
                            stored.map(String::as_str).unwrap_or("<unset>")
                        )),
                        found: Some(format!(
                            "{param}={}",
                            incoming.map(String::as_str).unwrap_or("<missing>")
                        )),
                    })
                }
            }
            Guard::And(guards) => guards
                .iter()
                .find_map(|g| g.check_detailed(current_state, ctx)),
        }
    }
}

/// Render the observed value of a boolean for a [`GuardFailure`].
fn found_bool(ctx: &EvalContext, var: &str) -> String {
    match ctx.booleans.get(var) {
        Some(true) => "true".to_string(),
        Some(false) => "false".to_string(),
        None => "<missing>".to_string(),
    }
}

#[cfg(test)]
#[path = "guard_test.rs"]
mod tests;
