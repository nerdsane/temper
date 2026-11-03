//! The joint model: the records in scope, their reactions, the dispatches
//! and module calls on their way, who may send what, and the walk.

mod commit;
mod drops;
mod initial;
mod labels;
mod order;
mod react;
mod read;
mod silent;
mod step;
mod walk;

use std::collections::{BTreeMap, BTreeSet};

use temper_authz::{May, SendAnalysis};
use temper_spec::automaton::Automaton;
use temper_spec::predicate::{Expr, Name, eval};

use super::env::{Blind, ID, JointEnv, Records};
use super::senders::{OUTSIDE, Senders};
use super::slice::slice;
use super::state::canonical;
use super::{JointOutcome, JointState};
use crate::model::semantics::{Expansion, rule_for, string_values};
use crate::model::{ModelString, ParamValue, ResolvedTransition, TemperModel, TemperModelState};

pub use drops::DroppedReaction;
pub(crate) use drops::dropped_reactions;
use order::{Order, Specs};
pub(super) use read::Timeout;
use read::{
    Kernel, Reaction, Unchecked, named_literals, reactions_into, tracked, unchecked_constraints,
};
pub(super) use step::Move;
pub(super) use walk::{Explored, Unwalked, unwalked_reason};

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
            kernel_moves: false,
            relies_on: BTreeSet::new(),
        }
    }
}

/// What a walk found.
pub(super) struct Walk {
    /// Distinct joint states walked.
    pub(super) states: usize,
    /// One per invariant.
    pub(super) outcomes: Vec<JointOutcome>,
    /// Whether a move was held back for [`MAX_IN_FLIGHT`].
    pub(super) held_back: bool,
}

/// The joint model of one entity's properties that read related entities.
#[derive(Debug)]
pub(super) struct JointModel {
    expansion: Expansion,
    id_literals: BTreeSet<String>,
    models: BTreeMap<String, TemperModel>,
    reactions: Vec<Reaction>,
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
    /// Each entity's transitions (by index) that nothing in the model sends
    /// and something in the runtime does, with who.
    unsent: BTreeMap<String, Vec<(usize, Unsent)>>,
}

/// Who in the runtime sends an action nothing in the model sends.
#[derive(Debug, Clone)]
enum Unsent {
    /// A module whose trigger names no `on_success`, while its call is open:
    /// the kernel takes whatever action the module's result names, and the
    /// model answers only with internal actions. Only for an action nothing
    /// else sends.
    Answer,
    /// Reactions from these types, outside the scope.
    Reactions(BTreeSet<String>),
}

impl Unsent {
    fn sender(&self) -> String {
        match self {
            Unsent::Answer => "a module whose trigger names no on_success".to_string(),
            Unsent::Reactions(from) => format!(
                "a reaction from {}",
                from.iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(" or ")
            ),
        }
    }
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
        let mut id_literals = BTreeSet::new();
        for entity in scope {
            let a = by_name[entity.as_str()];
            for inv in &a.invariants {
                read::collect_literals(&inv.assert, &mut id_literals);
            }
            for live in &a.liveness {
                if let Some(goal) = &live.eventually {
                    read::collect_literals(goal, &mut id_literals);
                }
            }
            for action in &a.actions {
                read::collect_literals(&action.guard, &mut id_literals);
                for trigger in &action.triggers {
                    if let Some(guard) = &trigger.guard {
                        read::collect_literals(guard, &mut id_literals);
                    }
                    for arg in trigger.args.values() {
                        if let temper_spec::predicate::Arg::Lit(
                            temper_spec::predicate::Literal::Str(s),
                        ) = arg
                        {
                            id_literals.insert(s.clone());
                        }
                    }
                }
            }
        }
        let kernel = if sending.kernel_moves {
            Kernel::read(by_name, scope)
        } else {
            Kernel::default()
        };
        let senders = Senders::new(by_name, scope, sending.policies);
        let chosen = |entity: &str, action: &str| {
            sending
                .relies_on
                .contains(&(entity.to_string(), action.to_string()))
                || senders.request(entity, action) != May::No
                || kernel.chooses(entity, action)
        };
        let order = Order::read(&Specs {
            by_name,
            models: &models,
            reactions: &reactions,
            read,
            chosen: &chosen,
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
        let sender_matters = reactions
            .iter()
            .filter(|r| r.principal.is_none())
            .map(|r| r.from.clone())
            .collect();
        let into = reactions_into(by_name, scope);
        let open: BTreeSet<&String> = kernel
            .calls_on
            .iter()
            .filter(|(_, specs)| specs.iter().any(|spec| spec.on_success.is_empty()))
            .map(|((entity, _), _)| entity)
            .collect();
        let unsent = models
            .iter()
            .filter_map(|(entity, model)| {
                let ways: Vec<(usize, Unsent)> = model
                    .transitions
                    .iter()
                    .enumerate()
                    .filter(|(_, t)| {
                        !chosen(entity, &t.name)
                            && !reactions
                                .iter()
                                .any(|r| &r.to == entity && r.target_action == t.name)
                    })
                    .filter_map(|(i, t)| match into.get(&(entity.clone(), t.name.clone())) {
                        Some(from) => Some((i, Unsent::Reactions(from.clone()))),
                        None => open.contains(entity).then_some((i, Unsent::Answer)),
                    })
                    .collect();
                (!ways.is_empty()).then(|| (entity.clone(), ways))
            })
            .collect();
        Self {
            expansion: Expansion::default(),
            id_literals,
            models,
            reactions,
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
        }
    }

    /// Note each move the runtime could make in `state` that the walk never
    /// takes.
    fn note_unwalked(&self, state: &JointState, unwalked: &mut BTreeSet<Unwalked>) {
        for (entity, ways) in &self.unsent {
            let open = state
                .calls
                .iter()
                .any(|call| &call.to == entity && call.on_success.is_empty());
            for (index, sender) in ways {
                let t = &self.models[entity].transitions[*index];
                if (open || !matches!(sender, Unsent::Answer))
                    && self.enabled(entity, t, state, Blind::Nothing)
                {
                    unwalked.insert(Unwalked {
                        action: format!("{entity}.{}", t.name),
                        sender: sender.sender(),
                        from: state.records[entity].status.clone(),
                    });
                }
            }
        }
    }

    /// The records as they start, without their ids.
    fn blank(&self) -> BTreeMap<String, TemperModelState> {
        self.models
            .iter()
            .map(|(entity, model)| {
                let init = stateright::Model::init_states(model)
                    .into_iter()
                    .next()
                    .expect("a model has one initial state");
                (entity.clone(), init)
            })
            .collect()
    }

    /// The starting state whose ids are `ids` (entity type to a trace name;
    /// equal names, equal ids; a type left out gets an id of its own), and
    /// each trace name's value in it.
    pub(super) fn init_with_ids(
        &self,
        ids: &BTreeMap<String, String>,
    ) -> (JointState, BTreeMap<String, ModelString>) {
        let mut names: BTreeMap<String, ModelString> = BTreeMap::new();
        let mut records = self.blank();
        for (entity, record) in &mut records {
            let name = ids
                .get(entity)
                .cloned()
                .unwrap_or_else(|| format!("{entity}#id"));
            let next = ModelString::Symbol(names.len() + 1);
            let value = names.entry(name).or_insert(next).clone();
            record.strings.insert(ID.to_string(), value);
        }
        let mut state = JointState {
            records,
            inflight: Vec::new(),
            calls: Vec::new(),
            armed: BTreeMap::new(),
        };
        self.arm_at_start(&mut state);
        self.order.normalize(&mut state);
        let renamed = canonical(&mut state);
        for value in names.values_mut() {
            if let Some(new) = renamed.get(value) {
                *value = new.clone();
            }
        }
        (state, names)
    }

    /// A record that starts in a state with a timer was made by someone
    /// outside, who armed it.
    fn arm_at_start(&self, state: &mut JointState) {
        for (entity, record) in &state.records {
            if self.timeout(entity, &record.status).is_some() {
                state.armed.insert(entity.clone(), OUTSIDE);
            }
        }
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
        let mut values = string_values(state.records.values());
        for dispatch in &state.inflight {
            for value in dispatch.params.values() {
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
        // The rule the runtime takes for this action in the record's status
        // (`rule_for`); another with the same name never fires from there.
        let taken = rule_for(&self.models[entity], &t.name, &state.records[entity].status)
            .is_some_and(|rule| std::ptr::eq(rule, t));
        taken && eval(&t.guard, &self.env_blind(entity, state, blind)).may_hold()
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
            state,
            owner,
            blind,
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
