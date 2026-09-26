//! Core types for the Temper verification model.
//!
//! Contains the state, action, transition, invariant, liveness, and model struct
//! definitions used by the Stateright model checker.

use std::collections::BTreeMap;
use std::fmt;

use temper_spec::predicate::Expr;

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
}

impl fmt::Display for TemperModelAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.target_state {
            Some(target) => write!(f, "{} -> {}", self.name, target),
            None => write!(f, "{}", self.name),
        }
    }
}

// ---------------------------------------------------------------------------
// Guards and effects — self-contained in temper-verify (mirror JIT types)
// ---------------------------------------------------------------------------

/// A state effect applied when a transition fires.
#[derive(Clone, Debug)]
pub enum ModelEffect {
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
    /// Precondition beyond `from_states`.
    pub guard: Expr,
    /// Effects applied when the transition fires.
    pub effects: Vec<ModelEffect>,
}

/// A safety invariant: `assert` must not be false in any reachable state.
#[derive(Clone, Debug)]
pub struct ResolvedInvariant {
    /// The invariant name.
    pub name: String,
    /// The assertion. Values the model does not track read as unknown, and
    /// an unknown result is not a violation.
    pub assert: Expr,
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
    /// States from which no transition may be enabled.
    pub terminal: Vec<String>,
    /// Declared state variable types, for evaluating guards and invariants.
    pub var_kinds: BTreeMap<String, temper_spec::predicate::VarKind>,
    /// The initial status (first state from Init, typically "Draft").
    pub(crate) initial_status: String,
    /// Initial counter values from [[state]] declarations.
    pub(crate) initial_counters: BTreeMap<String, usize>,
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
        };
        assert_eq!(a.to_string(), "Submit -> Active");
    }

    #[test]
    fn action_display_no_target() {
        let a = TemperModelAction {
            name: "AddItem".into(),
            target_state: None,
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
        };
        let json = serde_json::to_string(&a).unwrap();
        let back: TemperModelAction = serde_json::from_str(&json).unwrap();
        assert_eq!(back, a);
    }
}
