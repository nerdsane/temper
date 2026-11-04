//! The joint model: the records in scope, their reactions, the dispatches
//! and module calls on their way, who may send what, and the walk.

mod chosen;
mod classes;
mod commit;
pub(in crate::composite::joint) mod copies;
mod drops;
mod fair_key;
mod from_outside;
mod guided;
mod held;
mod initial;
mod labels;
mod moves;
mod order;
mod outside;
mod react;
mod read;
mod silent;
mod spawn;
mod step;
mod walk;

use std::collections::{BTreeMap, BTreeSet};

use temper_authz::{May, SendAnalysis};
use temper_spec::automaton::Automaton;
use temper_spec::predicate::{Expr, Name, eval};

use super::JointState;
use super::env::{Blind, ID, JointEnv, Records};
use super::senders::{OUTSIDE, Senders};
use super::slice::slice;
use super::state::canonical;
use crate::model::semantics::{Expansion, rule_for, string_values};
use crate::model::{ModelString, ParamValue, ResolvedTransition, TemperModel, TemperModelState};

pub(super) use classes::Classes;
pub use drops::DroppedReaction;
pub(crate) use drops::dropped_reactions;
pub(super) use fair_key::{FairKey, MoveKind};
pub(super) use from_outside::reads_beyond;
use from_outside::{Outside, outside_reactions};
pub use guided::{Candidate, Guide, GuidedSearch, STEPS as GUIDED_STEPS, guided_search, plain};
use order::{Order, Specs};
pub(super) use read::Timeout;
use read::{Kernel, Reaction, Unchecked, named_literals, tracked, unchecked_constraints};
pub(super) use step::{Certainty, Move};
pub(super) use walk::{Edge, Explored, Made};
pub(super) use walk::{Keep, Walk};

/// Counter ceiling for the joint models, as for the composite plans.
const JOINT_MAX_COUNTER: usize = 3;

/// Dispatches and module calls in flight at once. The runtime has no such
/// bound: each request, reaction and module call is its own task. The model
/// needs one to stay finite; three lets a late dispatch land after two later
/// ones. A walk that held a move back for it says so, and then proves only
/// up to this many.
pub(super) const MAX_IN_FLIGHT: usize = 3;

/// When a dispatch lands, against moves from outside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Delivery {
    /// Whenever: other dispatches and moves from outside can come first, as
    /// in the runtime, where each request and each reaction is its own task.
    /// At most [`MAX_IN_FLIGHT`] at once.
    Any,
    /// Before the next move from outside: the dispatches a move sends all
    /// land, in any order, before anything else is asked. That is what the
    /// drop check claims (ADR-0150); with it, the dispatches in flight are
    /// bounded by the reactions' fan-out and distinct steps.
    BeforeNextRequest,
}

/// Who sends what in the model.
pub(super) struct Sending<'s> {
    /// The policies that decide each move as its sender; `None` lets every
    /// sender send everything.
    pub(super) policies: Option<&'s SendAnalysis>,
    /// Reserve construction bytes before growing sender profiles or answer tables.
    pub(super) reserve: Option<&'s dyn Fn(usize) -> Result<(), String>>,
    /// Whether the model makes the kernel's own moves: state timers, module
    /// answers and failures, and module calls lost to a restart. Without
    /// them anyone may send any action, which covers what they do.
    pub(super) kernel_moves: bool,
    /// Actions (entity, action) that code outside the specs sends, offered
    /// and allowed whatever the policies say.
    pub(super) relies_on: BTreeSet<(String, String)>,
}

impl Sending<'_> {
    /// Anyone sends anything, and the kernel makes no moves of its own: what
    /// the drop check (ADR-0150) assumes, and what a recorded trace replays
    /// through.
    pub(super) fn anyone() -> Self {
        Self {
            policies: None,
            reserve: None,
            kernel_moves: false,
            relies_on: BTreeSet::new(),
        }
    }
}

/// The joint model of one entity's properties that read related entities.
#[derive(Debug)]
pub(super) struct JointModel {
    expansion: PerThread,
    /// The literals each type's id can meet, by type.
    id_literals: BTreeMap<String, BTreeSet<String>>,
    models: BTreeMap<String, TemperModel>,
    reactions: Vec<Reaction>,
    /// The names of each type's records ([`copies`]).
    copies: BTreeMap<String, Vec<String>>,
    /// Each reaction's place among the reactions as the specs say them.
    first_reaction: Vec<usize>,
    /// The reaction from its place in the specs, its source and its target.
    reaction_at: BTreeMap<(usize, String, String), usize>,
    records: Records,
    /// With one record of a type, each entity's id fields that references
    /// use: each holds that record's id.
    refs: BTreeMap<String, BTreeSet<String>>,
    delivery: Delivery,
    /// Whether the kernel's own moves are made as the kernel makes them.
    kernel_moves: bool,
    kernel: Kernel,
    relies_on: BTreeSet<(String, String)>,
    /// The actions with a constraint the model does not decide.
    unchecked: Unchecked,
    /// The counts kept as sequence numbers.
    order: Order,
    /// Who sends each move, and what Cedar lets them send.
    senders: Senders,
    /// The entities whose moves' senders matter: those with a reaction that
    /// runs as whoever sent its source action, which Cedar decides as them.
    /// Elsewhere who armed a timer or answers a call changes nothing, so the
    /// model does not tell them apart.
    sender_matters: BTreeSet<String>,
    /// The reactions (by place) whose guard a request's sender decides.
    chosen_guards: BTreeSet<usize>,
    /// Each entity's transitions (by index) that nothing in the model sends
    /// and a module may answer with: one whose trigger names no
    /// `on_success`, while its call is open. The kernel takes whatever
    /// action the module's result names, and the model answers only with
    /// internal actions.
    unsent: BTreeMap<String, Vec<usize>>,
    /// The actions (entity, action) of types in scope that a reaction from
    /// a type outside it sends ([`Outside`]).
    outside: BTreeMap<(String, String), Outside>,
    /// The fields a move (entity, action) stores a child's id in, when the
    /// child's type is outside the scope.
    outside_spawns: BTreeMap<(String, String), Vec<String>>,
    /// Each type's record as it first stands, made on first use.
    first: std::sync::OnceLock<BTreeMap<String, TemperModelState>>,
    /// Each type's statuses from which its record can send nothing more,
    /// made on first use.
    settled: std::sync::OnceLock<BTreeMap<String, BTreeSet<String>>>,
}

impl JointModel {
    /// The model of `owner`'s properties `reads` over `scope`. With `cut`,
    /// each entity is cut down to what `reads` depends on, which only adds
    /// behaviour.
    pub(super) fn new(
        by_name: &BTreeMap<&str, &Automaton>,
        scope: &BTreeSet<String>,
        owner: &str,
        reads: &[&Expr],
        sending: Sending<'_>,
        cut: bool,
    ) -> Self {
        // What the properties read: the owner's variables and the id fields
        // its references use, and the related types' fields.
        let mut read: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut owned = |entity: &str, field: &str| {
            if !matches!(field, "Id" | "id") {
                read.entry(entity.to_string())
                    .or_default()
                    .insert(field.to_string());
            }
        };
        for expr in reads {
            expr.for_each_name(&mut |n| match n {
                Name::Var(var) => owned(owner, var),
                Name::CrossStatus { id_field, .. } => owned(owner, id_field),
                Name::CrossField {
                    entity_type,
                    id_field,
                    field,
                } => {
                    owned(owner, id_field);
                    owned(entity_type, field);
                }
                // A move's parameter is no field of a record.
                Name::Status | Name::Param(_) => {}
            });
        }
        let (mut models, reactions) = tracked(by_name, scope, read.clone(), Records::ById);
        for ((entity, field), literals) in named_literals(owner, reads, &models, &reactions) {
            if let Some(model) = models.get_mut(&entity) {
                model
                    .string_literals
                    .entry(field)
                    .or_default()
                    .extend(literals);
            }
        }
        Self::compose(by_name, scope, (models, reactions), &read, sending, cut)
    }

    /// The model of `models` and `reactions` over `scope`, one record per
    /// id, its dispatches landing whenever. `read` is what its properties
    /// read; with `cut`, each entity is cut down to that.
    fn compose(
        by_name: &BTreeMap<&str, &Automaton>,
        scope: &BTreeSet<String>,
        (models, reactions): (BTreeMap<String, TemperModel>, Vec<Reaction>),
        read: &BTreeMap<String, BTreeSet<String>>,
        sending: Sending<'_>,
        cut: bool,
    ) -> Self {
        let copies = models
            .keys()
            .map(|entity| (entity.clone(), vec![entity.clone()]))
            .collect();
        let classes = Classes::read(by_name, scope);
        let id_literals = scope
            .iter()
            .map(|entity| (entity.clone(), classes.literals(entity, ID)))
            .collect();
        let kernel = if sending.kernel_moves {
            Kernel::read(by_name, scope)
        } else {
            Kernel::default()
        };
        // A reaction that declares no principal runs as whoever sent its
        // source action.
        let inherits = reactions.iter().filter(|r| r.principal.is_none());
        let sender_matters: BTreeSet<String> = inherits.clone().map(|r| r.from.clone()).collect();
        let inheriting = inherits
            .map(|r| (r.to.clone(), r.target_action.clone()))
            .collect();
        let senders = Senders::new(
            by_name,
            scope,
            sending.policies,
            &sender_matters,
            &inheriting,
            sending.reserve,
        );
        // A reaction from outside the scope chooses its parameters, as requests do.
        let outside = outside_reactions(by_name, scope);
        let chosen = |entity: &str, action: &str| {
            let key = (entity.to_string(), action.to_string());
            sending.relies_on.contains(&key)
                || outside.contains_key(&key)
                || senders.request(entity, action) != May::No
                || kernel.chooses(entity, action)
        };
        let open: BTreeSet<&String> = kernel
            .calls_on
            .iter()
            .filter(|(_, specs)| specs.iter().any(|spec| spec.on_success.is_empty()))
            .map(|((entity, _), _)| entity)
            .collect();
        // A module whose trigger names no `on_success` may answer with an
        // action of its record nothing else sends, choosing its parameters.
        let only_a_module = |entity: &str, action: &str| {
            open.contains(&entity.to_string())
                && !chosen(entity, action)
                && !reactions
                    .iter()
                    .any(|r| r.to == entity && r.target_action == action)
        };
        let answered =
            |entity: &str, action: &str| chosen(entity, action) || only_a_module(entity, action);
        let order = Order::read(&Specs {
            by_name,
            models: &models,
            reactions: &reactions,
            read,
            chosen: &answered,
        });
        let models: BTreeMap<String, TemperModel> = models
            .into_iter()
            .map(|(entity, model)| {
                let mut keep = read.get(&entity).cloned().unwrap_or_default();
                keep.extend(order.fields(&entity).map(|(field, _)| field.to_string()));
                keep.extend(deciding(by_name[entity.as_str()], &reactions, &entity));
                let mut model = if cut { slice(model, keep) } else { model };
                order.bound(&entity, &mut model);
                (entity, model)
            })
            .collect();
        let unchecked = unchecked_constraints(by_name, &models, &order);
        let unsent = models
            .iter()
            .filter(|(entity, _)| open.contains(entity))
            .filter_map(|(entity, model)| {
                let ways: Vec<usize> = model
                    .transitions
                    .iter()
                    .enumerate()
                    .filter(|(_, t)| only_a_module(entity, &t.name))
                    .map(|(i, _)| i)
                    .collect();
                (!ways.is_empty()).then(|| (entity.clone(), ways))
            })
            .collect();
        // Cedar is told which literals a made-up value may not be.
        let senders = senders.meeting(&classes, &models);
        Self {
            expansion: PerThread::default(),
            id_literals,
            models,
            chosen_guards: chosen::chosen_guards(by_name, &reactions),
            reactions,
            copies,
            first_reaction: Vec::new(),
            reaction_at: BTreeMap::new(),
            records: Records::ById,
            refs: BTreeMap::new(),
            delivery: Delivery::Any,
            kernel_moves: sending.kernel_moves,
            kernel,
            relies_on: sending.relies_on,
            unchecked,
            order,
            senders,
            sender_matters,
            unsent,
            outside,
            outside_spawns: spawn::outside_spawns(by_name, scope),
            first: std::sync::OnceLock::new(),
            settled: std::sync::OnceLock::new(),
        }
    }

    /// Retain the finished profiles and answer tables, or propagate construction failure.
    pub(super) fn built(self, memory: &super::room::Construction<'_>) -> Result<Self, String> {
        if let Some(reason) = &self.senders.problem {
            memory.set(0)?;
            return Err(reason.clone());
        }
        memory.set(self.senders.construction_bytes())?;
        Ok(self)
    }

    /// `entity`'s timer in `status`, when the model makes timers fire.
    pub(super) fn timeout(&self, entity: &str, status: &str) -> Option<&Timeout> {
        self.kernel
            .timers
            .get(entity)?
            .iter()
            .find(|timeout| timeout.state == status)
    }

    /// The strings `state` holds, in records and in dispatches.
    pub(super) fn values(&self, state: &JointState) -> BTreeSet<ModelString> {
        let mut values = string_values(state.records.values().map(|record| &**record));
        for dispatch in &state.inflight {
            for value in dispatch.values() {
                if let ParamValue::Str(text) = value
                    && !text.is_empty()
                {
                    values.insert(text.clone());
                }
            }
        }
        for call in &state.calls {
            values.extend(call.asked.values().filter(|v| !v.is_empty()).cloned());
        }
        values
    }

    /// Whether `t`'s guard reads a related record in scope.
    fn reads_related(&self, t: &ResolvedTransition) -> bool {
        t.guard
            .related_types()
            .iter()
            .any(|(entity, _)| self.models.contains_key(entity))
    }

    /// Whether some transition named `action` of `entity` reads a related
    /// record.
    fn action_reads_related(&self, entity: &str, action: &str) -> bool {
        self.models[entity]
            .transitions
            .iter()
            .any(|t| t.name == action && self.reads_related(t))
    }

    fn enabled(
        &self,
        entity: &str,
        t: &ResolvedTransition,
        state: &JointState,
        blind: Blind,
    ) -> bool {
        self.enabled_carrying(entity, t, state, blind, None)
    }

    /// [`Self::enabled`] for a dispatch carrying `params`.
    fn enabled_carrying(
        &self,
        entity: &str,
        t: &ResolvedTransition,
        state: &JointState,
        blind: Blind,
        params: Option<&BTreeMap<String, ParamValue>>,
    ) -> bool {
        // The rule the runtime takes for this action in the record's status
        // (`rule_for`); another with the same name never fires from there.
        let taken = rule_for(&self.models[entity], &t.name, &state.records[entity].status)
            .is_some_and(|rule| std::ptr::eq(rule, t));
        let env = JointEnv {
            params,
            ..self.env_blind(entity, state, blind)
        };
        taken && eval(&t.guard, &env).may_hold()
    }

    pub(super) fn env<'a>(&'a self, owner: &'a str, state: &'a JointState) -> JointEnv<'a> {
        self.env_blind(owner, state, Blind::Nothing)
    }

    fn env_blind<'a>(
        &'a self,
        owner: &'a str,
        state: &'a JointState,
        blind: Blind,
    ) -> JointEnv<'a> {
        JointEnv {
            models: &self.models,
            records: self.records,
            refs: &self.refs,
            copies: &self.copies,
            state,
            owner,
            blind,
            own: None,
            unheld_unknown: false,
            params: None,
        }
    }
}

/// The fields of an `entity` record that decide whether a move happens
/// without being a guard: those a constraint compares a parameter with, and
/// those the guard of a trigger it fires reads. A cut keeps them, so a
/// record refusing a dispatch for a stale count, or a reaction it does not
/// send, stays decided.
fn deciding(automaton: &Automaton, reactions: &[Reaction], entity: &str) -> BTreeSet<String> {
    let mut fields: BTreeSet<String> = automaton
        .actions
        .iter()
        .flat_map(|action| &action.constraints)
        .filter_map(|constraint| constraint.field().map(str::to_string))
        .collect();
    for guard in reactions
        .iter()
        .filter(|r| r.from == entity)
        .filter_map(|r| r.guard.as_ref())
    {
        guard.for_each_name(&mut |name| {
            if let Name::Var(var) = name {
                fields.insert(var.to_string());
            }
        });
    }
    fields
}

/// A walk's accounting for what one state's expansion builds, one to a
/// thread, since a walk's threads each expand their own state.
#[derive(Default)]
struct PerThread(thread_local::ThreadLocal<Expansion>);

impl std::ops::Deref for PerThread {
    type Target = Expansion;

    fn deref(&self) -> &Expansion {
        self.0.get_or_default()
    }
}

/// Only the name, as [`Expansion`]'s own says: it is no part of the model.
impl std::fmt::Debug for PerThread {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Expansion")
    }
}
