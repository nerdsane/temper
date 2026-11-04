//! Core types for the Temper verification model.
//!
//! Contains the state, action, transition, invariant, liveness, and model struct
//! definitions used by the Stateright model checker.

pub use super::string::ModelString;

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use std::collections::BTreeSet;

use temper_spec::predicate::{Arg, Expr, ParamKind};

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
    /// Named list variables (e.g. `tags`, `labels`). Elements retain equality
    /// symbols, so reused inputs alias and independent inputs can differ.
    pub lists: BTreeMap<String, Vec<ModelString>>,
    /// The string variables guards or rules read or effects write, as symbols: `''`, a literal
    /// the spec compares the variable with, or `#n`, a value that is equal
    /// only to itself. The verifier tracks which strings are equal, not what
    /// they say, so `#n` are renumbered in order of first use after every
    /// step ([`super::semantics::canonical_strings`]). Empty for a spec whose
    /// guards, rules and effects use no string.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub strings: BTreeMap<String, ModelString>,
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
        if !self.strings.is_empty() {
            let pairs: Vec<String> = self
                .strings
                .iter()
                .map(|(k, v)| format!("{k}='{v}'"))
                .collect();
            write!(f, "<{}>", pairs.join(","))?;
        }
        Ok(())
    }
}

/// The value the model chose for an action parameter in one step.
#[derive(
    Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum ParamValue {
    /// A non-negative integer.
    Count(usize),
    /// A boolean.
    Bool(bool),
    /// A concrete string or a fresh equality symbol.
    Str(ModelString),
    /// A string no guard or invariant mentions.
    Fresh,
}

impl fmt::Display for ParamValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParamValue::Count(n) => write!(f, "{n}"),
            ParamValue::Bool(b) => write!(f, "{b}"),
            ParamValue::Str(s) => write!(f, "'{s}'"),
            ParamValue::Fresh => f.write_str("<fresh>"),
        }
    }
}

/// An action that the model can take, corresponding to a specification transition.
#[derive(Clone, Debug, Hash, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TemperModelAction {
    /// The transition name (e.g. "SubmitOrder", "CancelOrder").
    pub name: String,
    /// The target status after taking this action (if deterministic).
    pub target_state: Option<String>,
    /// The values chosen for the parameters the transition's effects read.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, ParamValue>,
}

impl fmt::Display for TemperModelAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name)?;
        if !self.params.is_empty() {
            let pairs: Vec<String> = self
                .params
                .iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect();
            write!(f, "({})", pairs.join(", "))?;
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

/// A state effect applied when a transition fires. `params.p` values are
/// chosen per step (see [`TemperModelAction::params`]).
#[derive(Clone, Debug)]
pub enum ModelEffect {
    /// `var = value` on a counter.
    SetCounter { var: String, value: Arg },
    /// `var += value` on a counter.
    AddCounter { var: String, value: Arg },
    /// `var -= value` on a counter (saturating).
    SubCounter { var: String, value: Arg },
    /// `var = value` on a bool.
    SetBool { var: String, value: Arg },
    /// `var = 'literal'` on a string, including the empty string.
    SetString { var: String, value: String },
    /// `append(var, value)`.
    ListAppend { var: String, value: Arg },
    /// `remove_at(var, index)`; out of range is a no-op.
    ListRemoveAt { var: String, index: Arg },
    /// `remove(var, value)`: every element equal to `value`; none is a no-op.
    ListRemove { var: String, value: Arg },
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
    /// The action parameters the effects read, by kind. Entry-rule models
    /// also choose parameters read only by guards or constraints.
    pub params: BTreeMap<String, ParamKind>,
    /// The string parameters the model gives a value: those that write a
    /// tracked string variable of the same name (the runtime writes every
    /// parameter into its field), and those a constraint ties to a tracked
    /// one. Entry-rule models also choose guard inputs and list inputs here
    /// to retain identity and evaluate the guard with the chosen assignment.
    pub string_params: Vec<StringParam>,
    /// Original input constraints. Route proofs must identify any that
    /// string parameter exploration does not enforce.
    pub constraints: Vec<temper_spec::automaton::ActionConstraint>,
}

/// A string parameter of an action, with what the action's constraints allow
/// it to be.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StringParam {
    /// The parameter.
    pub name: String,
    /// Whether it writes the tracked string variable of its name. One that
    /// does not is modelled only so its constraints are decided.
    pub writes: bool,
    /// `param_nonempty`: never empty or whitespace-only.
    pub nonempty: bool,
    /// `param_equals_field`: equal to the value of every one of these variables.
    pub equals: Vec<String>,
    /// `param_not_equals_field`: never the value of these variables.
    pub not_equals: Vec<String>,
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

/// A rule on entering a place: every move into `place` from one of `from`
/// must have `assert` true just before it fires (`model/entry.rs`).
#[derive(Clone, Debug)]
pub struct ResolvedEntryRule {
    /// The rule name.
    pub name: String,
    /// The place it guards.
    pub place: String,
    /// The places whose moves into `place` it covers, never `place` itself
    /// unless the spec names it.
    pub from: Vec<String>,
    /// The assertion, read in the state before the move.
    pub assert: Expr,
}

// ---------------------------------------------------------------------------
// Liveness properties
// ---------------------------------------------------------------------------

/// The kind of liveness property.
#[derive(Clone, Debug)]
pub enum LivenessKind {
    /// From every reachable state in one of `from` (a new record when it is
    /// empty), every path gets to one of `targets`: none stops short or
    /// loops forever without them. Checked on the model check's whole walk,
    /// after a complete exploration ([`crate::reaches`]).
    ReachesState {
        from: Vec<String>,
        targets: Vec<String>,
    },
    /// From any `from` state, there is always at least one enabled action.
    /// This is actually a safety property (always has actions).
    NoDeadlock { from: Vec<String> },
    /// From every reachable state in one of `from` (any state when it is
    /// empty), some moves lead to one of `targets`. Checked on the model
    /// check's whole walk, after a complete exploration.
    CanReach {
        from: Vec<String>,
        targets: Vec<String>,
    },
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
    /// Rules every move into a place must meet as it fires.
    pub entry_rules: Vec<ResolvedEntryRule>,
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
    pub(crate) initial_lists: BTreeMap<String, Vec<ModelString>>,
    /// Initial values of the tracked string variables.
    pub(crate) initial_strings: BTreeMap<String, ModelString>,
    /// The string variables the verifier tracks: those guards/invariants read
    /// or effects write, and those whose value reaches one through a
    /// constraint or a reaction.
    pub tracked_strings: BTreeSet<String>,
    /// Concrete initial, predicate and effect literals for tracked string inputs,
    /// including literals tied to them indirectly through related statuses.
    pub(crate) string_literals: BTreeMap<String, BTreeSet<String>>,
    /// Whether `string_literals` is all a parameter can meet: a literal held
    /// elsewhere is a value like any made-up one to whatever reads it.
    pub(crate) literals_by_class: bool,
    /// Per-counter upper bounds for bounded exploration.
    pub(crate) counter_bounds: BTreeMap<String, usize>,
    /// Default upper bound for counters not in counter_bounds.
    pub(crate) default_max_counter: usize,
    /// Per-list upper bounds on length for bounded exploration; a list not
    /// here is explored to `default_max_counter`.
    pub(crate) list_bounds: BTreeMap<String, usize>,
    /// String literals each list is compared against (`'x' in list`) in any
    /// guard or invariant: the values worth choosing for `append(list, params.p)`.
    pub(crate) list_literals: BTreeMap<String, BTreeSet<String>>,
    /// How far this model may be explored, when a budgeted cascade built it.
    /// `None` explores to the end.
    pub(crate) limit: Option<ExploreLimit>,
}

/// How far one model may be explored: a [`crate::Budget`] in force.
#[derive(Clone, Debug)]
pub(crate) struct ExploreLimit {
    /// The time the budget has used. Past `time` the model offers no
    /// actions, so every walk of it (model check, simulation, property
    /// tests) ends at once.
    pub(crate) elapsed: crate::budget::Elapsed,
    /// The budget's time.
    pub(crate) time: std::time::Duration,
    /// States the model check may generate before it stops (a state reached
    /// twice counts twice).
    pub(crate) states: usize,
    /// Set once a walk is over, so the model offers its actions again:
    /// Stateright rebuilds a found path by stepping the model from the start.
    pub(crate) lifted: Arc<AtomicBool>,
    /// Remaining expansion work shared by all callers in this walk.
    pub(crate) work: Arc<AtomicUsize>,
    /// Expansion stopped before all assignments were generated.
    pub(crate) exhausted: Arc<AtomicBool>,
}

impl ExploreLimit {
    /// The same limit for one walk of its own, lifted without lifting it for
    /// the other walks of the same budget.
    pub(crate) fn for_one_walk(&self) -> Self {
        Self {
            lifted: Arc::new(AtomicBool::new(false)),
            work: Arc::new(AtomicUsize::new(self.states)),
            exhausted: Arc::new(AtomicBool::new(false)),
            ..self.clone()
        }
    }

    pub(crate) fn lift(&self) {
        self.lifted.store(true, Ordering::Relaxed);
    }

    /// Whether the budget's time is spent.
    pub(crate) fn past_time(&self) -> bool {
        self.elapsed.now() >= self.time
    }
}

/// Every field a walk reads, in order. `limit` is left out: it is the budget a
/// walk runs under, not part of the model, so a model prints the same however
/// it is budgeted. The checks are kept by a digest of this text
/// ([`crate::VerificationCascade::digest`]); the destructuring makes a new
/// field a compile error here until it is printed or left out on purpose.
impl fmt::Debug for TemperModel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let TemperModel {
            states,
            transitions,
            invariants,
            entry_rules,
            liveness,
            terminal,
            var_kinds,
            initial_status,
            initial_counters,
            initial_booleans,
            initial_lists,
            initial_strings,
            tracked_strings,
            string_literals,
            literals_by_class,
            counter_bounds,
            default_max_counter,
            list_bounds,
            list_literals,
            limit: _,
        } = self;
        f.debug_struct("TemperModel")
            .field("states", states)
            .field("transitions", transitions)
            .field("invariants", invariants)
            .field("entry_rules", entry_rules)
            .field("liveness", liveness)
            .field("terminal", terminal)
            .field("var_kinds", var_kinds)
            .field("initial_status", initial_status)
            .field("initial_counters", initial_counters)
            .field("initial_booleans", initial_booleans)
            .field("initial_lists", initial_lists)
            .field("initial_strings", initial_strings)
            .field("tracked_strings", tracked_strings)
            .field("string_literals", string_literals)
            .field("literals_by_class", literals_by_class)
            .field("counter_bounds", counter_bounds)
            .field("default_max_counter", default_max_counter)
            .field("list_bounds", list_bounds)
            .field("list_literals", list_literals)
            .finish_non_exhaustive()
    }
}

impl TemperModel {
    /// Whether the model holds a value for the counter, boolean or list
    /// `field`: some rule reads it.
    pub fn holds(&self, field: &str) -> bool {
        self.initial_counters.contains_key(field)
            || self.initial_booleans.contains_key(field)
            || self.initial_lists.contains_key(field)
    }

    /// Whether the model gives `action`'s string parameter `param` a value.
    pub fn models_string(&self, action: &str, param: &str) -> bool {
        self.transitions
            .iter()
            .filter(|t| t.name == action)
            .any(|t| t.string_params.iter().any(|p| p.name == param))
    }

    /// Whether the model has spent its time or expansion work allowance.
    pub fn out_of_time(&self) -> bool {
        self.limit.as_ref().is_some_and(|limit| {
            !limit.lifted.load(Ordering::Relaxed)
                && (limit.past_time() || limit.exhausted.load(Ordering::Relaxed))
        })
    }
}
