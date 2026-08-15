//! Core types for the Temper verification model.
//!
//! Contains the state, action, transition, invariant, liveness, and model struct
//! definitions used by the Stateright model checker.

use std::collections::BTreeMap;
use std::fmt;

/// The state tracked by the Temper model during verification.
///
/// Multi-variable state: status + named counters + named booleans.
/// This generalises the old `(status, item_count)` to handle arbitrary
/// IOA `[[state]]` variable declarations.
#[derive(Clone, Debug, Hash, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TemperModelState {
    /// Current status value (mirrors the specification's `status` variable).
    pub status: String,
    /// Named counter variables (e.g. `items`, `quantity`).
    pub counters: BTreeMap<String, usize>,
    /// Named boolean variables (e.g. `has_address`, `payment_captured`).
    pub booleans: BTreeMap<String, bool>,
    /// Named list variables (e.g. `tags`, `labels`).
    pub lists: BTreeMap<String, Vec<String>>,
}

impl fmt::Display for TemperModelState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.status)?;
        if !self.counters.is_empty() {
            let pairs: Vec<String> = self
                .counters
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect();
            write!(f, "({})", pairs.join(","))?;
        }
        if !self.booleans.is_empty() {
            let pairs: Vec<String> = self
                .booleans
                .iter()
                .filter(|(_, v)| **v)
                .map(|(k, _)| k.clone())
                .collect();
            if !pairs.is_empty() {
                write!(f, "[{}]", pairs.join(","))?;
            }
        }
        if !self.lists.is_empty() {
            let pairs: Vec<String> = self
                .lists
                .iter()
                .map(|(k, v)| format!("{k}#{}", v.len()))
                .collect();
            write!(f, "{{{}}}", pairs.join(","))?;
        }
        Ok(())
    }
}

/// An action that the model can take, corresponding to a specification transition.
#[derive(Clone, Debug, Hash, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TemperModelAction {
    /// The transition name (e.g. "SubmitOrder", "CancelOrder").
    pub name: String,
    /// The target status after taking this action (if deterministic).
    pub target_state: Option<String>,
    /// Finite typed-reference parameter identity classes for this branch.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub reference_params: BTreeMap<String, u8>,
    /// Declaration-ordered identity classes reserved for atomic creates.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fresh_references: BTreeMap<String, Vec<u8>>,
}

impl fmt::Display for TemperModelAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name)?;
        if !self.reference_params.is_empty() {
            let params = self
                .reference_params
                .iter()
                .map(|(name, symbol)| format!("{name}={symbol}"))
                .collect::<Vec<_>>()
                .join(",");
            write!(f, "[ref:{params}]")?;
        }
        if !self.fresh_references.is_empty() {
            let fresh = self
                .fresh_references
                .iter()
                .map(|(target, symbols)| format!("{target}={symbols:?}"))
                .collect::<Vec<_>>()
                .join(",");
            write!(f, "[fresh:{fresh}]")?;
        }
        if let Some(target) = &self.target_state {
            write!(f, " -> {target}")?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Guards and effects — self-contained in temper-verify (mirror JIT types)
// ---------------------------------------------------------------------------

/// A guard condition for model checking.
///
/// Self-contained in temper-verify so we don't depend on temper-jit types.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelGuard {
    /// Always enabled (no guard).
    Always,
    /// Current status must be in the given set.
    StateIn(Vec<String>),
    /// A counter variable must be >= min.
    CounterMin { var: String, min: usize },
    /// A counter variable must be < max.
    CounterMax { var: String, max: usize },
    /// A boolean variable must be true.
    BoolTrue(String),
    /// A boolean variable must be false.
    BoolFalse(String),
    /// A list variable must contain a value.
    ListContains { var: String, value: String },
    /// A list variable must have at least N elements.
    ListLengthMin { var: String, min: usize },
    /// A cross-entity status precondition: the related entity must be in
    /// `required_status` (allowlist; empty ⇒ unconstrained) and NOT in
    /// `forbidden_status` (denylist; empty ⇒ unconstrained).
    ///
    /// The single-entity verifier cannot resolve this guard from local state.
    /// Backends treat it as an abstract guard rather than erasing it. The
    /// composite verifier, where the target is in scope, resolves both lists
    /// concretely against the target's slice.
    CrossEntityState {
        entity_type: String,
        entity_id_source: String,
        required_status: Vec<String>,
        forbidden_status: Vec<String>,
    },
    /// An incoming typed reference must equal a stored typed reference.
    /// Parameter identities are explored symbolically by verification backends.
    ReferenceEquals { reference: String, param: String },
    /// All sub-guards must hold.
    And(Vec<ModelGuard>),
}

impl ModelGuard {
    /// Returns true when this guard depends on state outside the current entity.
    pub fn contains_cross_entity(&self) -> bool {
        match self {
            ModelGuard::CrossEntityState { .. } => true,
            ModelGuard::And(guards) => guards.iter().any(ModelGuard::contains_cross_entity),
            _ => false,
        }
    }
}

/// A state effect applied when a transition fires.
#[derive(Clone, Debug)]
pub enum ModelEffect {
    /// Explore the finite identity classes for an incoming typed parameter.
    ExploreReferenceParam { param: String, entity_type: String },
    /// Project a typed reference parameter into an immutable reference field.
    SetReferenceFromParam { reference: String, param: String },
    /// Bind or validate the deterministic identity tuple after projection.
    EnforceIdentity(Vec<String>),
    /// Reserve fresh identity classes created by atomic composite sub-writes.
    ReserveFreshReferences { entity_type: String, count: usize },
    /// Canonicalize stored reference symbols in declaration order.
    CanonicalizeReferences(BTreeMap<String, Vec<String>>),
    /// Increment a counter variable by 1.
    IncrementCounter(String),
    /// Decrement a counter variable by 1 (saturating).
    DecrementCounter(String),
    /// Set a boolean variable to a value.
    SetBool { var: String, value: bool },
    /// Append a value to a list variable.
    ListAppend(String),
    /// Remove one value from a list variable.
    ListRemoveAt(String),
}

/// A resolved transition used internally by the model, pre-computed from a
/// specification action for efficient matching during state exploration.
#[derive(Clone, Debug)]
pub struct ResolvedTransition {
    /// The action name.
    pub name: String,
    /// States from which this transition can fire.
    pub from_states: Vec<String>,
    /// The target state (if deterministic).
    pub to_state: Option<String>,
    /// Guard condition (beyond status check).
    pub guard: ModelGuard,
    /// Effects applied when the transition fires.
    pub effects: Vec<ModelEffect>,
}

/// The kind of check a safety invariant performs.
#[derive(Clone, Debug)]
pub enum InvariantKind {
    /// Status must be in a known set of states (TypeInvariant).
    StatusInSet,
    /// When status is in trigger_states, a counter must be > 0.
    CounterPositive { var: String },
    /// When status is in trigger_states, a boolean must match `expect`.
    ///
    /// `expect = true` encodes `flag`; `expect = false` encodes `!flag`.
    BoolRequired { var: String, expect: bool },
    /// When status is in trigger_states, no transitions should be enabled.
    NoFurtherTransitions,
    /// When status is in trigger_states, status must also be in required_states.
    Implication,
    /// Generalized counter comparison (e.g., `items >= 1`, `retries < 5`).
    CounterCompare {
        var: String,
        op: temper_spec::automaton::AssertCompareOp,
        value: usize,
    },
    /// The entity should never be in this state.
    NeverState { state: String },
    /// Compound: all subexpressions must hold.
    And(Vec<InvariantKind>),
    /// Compound: at least one subexpression must hold.
    Or(Vec<InvariantKind>),
    /// Assertion expression that cannot be verified at model level.
    /// Surfaces as a warning in the cascade result.
    Unverifiable { expression: String },
}

/// A safety invariant resolved for runtime checking.
#[derive(Clone, Debug)]
pub struct ResolvedInvariant {
    /// The invariant name.
    pub name: String,
    /// States in which this invariant's check is activated (empty = always).
    pub trigger_states: Vec<String>,
    /// For implication invariants: the set of valid target states.
    pub required_states: Vec<String>,
    /// The kind of check this invariant performs.
    pub kind: InvariantKind,
}

// ---------------------------------------------------------------------------
// Liveness properties
// ---------------------------------------------------------------------------

/// The kind of liveness property.
#[derive(Clone, Debug)]
pub enum LivenessKind {
    /// From any `from` state, eventually reaches one of `targets`.
    /// Checked via Stateright's `Property::eventually` (acyclic paths only).
    ReachesState {
        from: Vec<String>,
        targets: Vec<String>,
    },
    /// From any `from` state, there is always at least one enabled action.
    /// This is actually a safety property (always has actions).
    NoDeadlock { from: Vec<String> },
}

/// A resolved liveness property.
#[derive(Clone, Debug)]
pub struct ResolvedLiveness {
    /// The property name.
    pub name: String,
    /// The kind of liveness check.
    pub kind: LivenessKind,
}

/// The Stateright model generated from an I/O Automaton specification.
///
/// This struct holds all the pre-computed transition, invariant, and liveness
/// data needed to implement the `Model` trait efficiently. Invariant data is
/// stored here (rather than captured in closures) because Stateright's
/// `Property::always` requires a bare `fn` pointer.
#[derive(Clone)]
pub struct TemperModel {
    /// All valid status values from the specification.
    pub states: Vec<String>,
    /// Pre-resolved transitions.
    pub transitions: Vec<ResolvedTransition>,
    /// Pre-resolved safety invariants (accessible to property fn pointers via &self).
    pub invariants: Vec<ResolvedInvariant>,
    /// Pre-resolved liveness properties.
    pub liveness: Vec<ResolvedLiveness>,
    /// The initial status (first state from Init, typically "Draft").
    pub(crate) initial_status: String,
    /// Initial counter values from [[state]] declarations.
    pub(crate) initial_counters: BTreeMap<String, usize>,
    /// Canonical identity-equivalence variants for already-created entities.
    pub(crate) initial_counter_variants: Vec<BTreeMap<String, usize>>,
    /// Typed reference properties grouped by target entity type.
    pub(crate) reference_properties_by_type: BTreeMap<String, Vec<String>>,
    /// Declaration-ordered deterministic identity properties, when configured.
    pub(crate) identity_properties: Vec<String>,
    /// Initial boolean values from [[state]] declarations.
    pub(crate) initial_booleans: BTreeMap<String, bool>,
    /// Initial list values from [[state]] declarations.
    pub(crate) initial_lists: BTreeMap<String, Vec<String>>,
    /// Per-counter upper bounds for bounded exploration.
    pub(crate) counter_bounds: BTreeMap<String, usize>,
    /// Default upper bound for counters not in counter_bounds.
    pub(crate) default_max_counter: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_display_status_only() {
        let s = TemperModelState {
            status: "Draft".into(),
            counters: BTreeMap::new(),
            booleans: BTreeMap::new(),
            lists: BTreeMap::new(),
        };
        assert_eq!(s.to_string(), "Draft");
    }

    #[test]
    fn state_display_with_counters() {
        let s = TemperModelState {
            status: "Active".into(),
            counters: BTreeMap::from([("items".into(), 3)]),
            booleans: BTreeMap::new(),
            lists: BTreeMap::new(),
        };
        assert_eq!(s.to_string(), "Active(items=3)");
    }

    #[test]
    fn state_display_with_true_booleans() {
        let s = TemperModelState {
            status: "Active".into(),
            counters: BTreeMap::new(),
            booleans: BTreeMap::from([("ready".into(), true), ("done".into(), false)]),
            lists: BTreeMap::new(),
        };
        assert_eq!(s.to_string(), "Active[ready]");
    }

    #[test]
    fn state_display_with_lists() {
        let s = TemperModelState {
            status: "Active".into(),
            counters: BTreeMap::new(),
            booleans: BTreeMap::new(),
            lists: BTreeMap::from([("tags".into(), vec!["a".into(), "b".into()])]),
        };
        assert_eq!(s.to_string(), "Active{tags#2}");
    }

    #[test]
    fn state_display_full() {
        let s = TemperModelState {
            status: "Active".into(),
            counters: BTreeMap::from([("items".into(), 2)]),
            booleans: BTreeMap::from([("ready".into(), true)]),
            lists: BTreeMap::from([("tags".into(), vec!["x".into()])]),
        };
        assert_eq!(s.to_string(), "Active(items=2)[ready]{tags#1}");
    }

    #[test]
    fn state_display_all_booleans_false_hides_brackets() {
        let s = TemperModelState {
            status: "X".into(),
            counters: BTreeMap::new(),
            booleans: BTreeMap::from([("a".into(), false), ("b".into(), false)]),
            lists: BTreeMap::new(),
        };
        assert_eq!(s.to_string(), "X");
    }

    #[test]
    fn action_display_with_target() {
        let a = TemperModelAction {
            name: "Submit".into(),
            target_state: Some("Active".into()),
            reference_params: BTreeMap::new(),
            fresh_references: BTreeMap::new(),
        };
        assert_eq!(a.to_string(), "Submit -> Active");
    }

    #[test]
    fn action_display_no_target() {
        let a = TemperModelAction {
            name: "AddItem".into(),
            target_state: None,
            reference_params: BTreeMap::new(),
            fresh_references: BTreeMap::new(),
        };
        assert_eq!(a.to_string(), "AddItem");
    }

    #[test]
    fn state_serde_roundtrip() {
        let s = TemperModelState {
            status: "Draft".into(),
            counters: BTreeMap::from([("items".into(), 5)]),
            booleans: BTreeMap::from([("ready".into(), true)]),
            lists: BTreeMap::from([("tags".into(), vec!["vip".into()])]),
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: TemperModelState = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn action_serde_roundtrip() {
        let a = TemperModelAction {
            name: "Submit".into(),
            target_state: Some("Active".into()),
            reference_params: BTreeMap::new(),
            fresh_references: BTreeMap::new(),
        };
        let json = serde_json::to_string(&a).unwrap();
        let back: TemperModelAction = serde_json::from_str(&json).unwrap();
        assert_eq!(back, a);
    }
}
