//! What the joint model reads once from the specs: each type's model with
//! the strings it must track, the reactions between the types in scope, the
//! module calls their actions start, their internal actions and state
//! timers, and the constraints it does not check.

use std::collections::{BTreeMap, BTreeSet};

use temper_authz::Asker;
use temper_spec::automaton::{
    ActionConstraint, ActionKind, Automaton, TargetResolver, TriggerKind,
};
use temper_spec::predicate::{Arg, CmpOp, Expr, Literal, Name, Operand, Set, VarKind};

use super::super::env::Records;
use super::super::senders;
use super::JOINT_MAX_COUNTER;
use super::order::Order;
use crate::model::{TemperModel, build_model_tracking};

/// An entity reaction between two types in scope, with the arguments it
/// passes (target parameter to a source field or a literal).
#[derive(Debug, Clone)]
pub(super) struct Reaction {
    pub(super) from: String,
    pub(super) source_action: String,
    pub(super) to: String,
    pub(super) target_action: String,
    pub(super) guard: Option<Expr>,
    pub(super) args: BTreeMap<String, Arg>,
    pub(super) resolve: Option<TargetResolver>,
    /// The service it runs as, when it declares one.
    pub(super) principal: Option<Asker>,
    pub(super) trigger: String,
    /// Whether the runtime may drop it without the drop check saying so: it
    /// creates its target (always ready for it), or `drop_ok` says a drop is
    /// meant.
    pub(super) may_drop: bool,
}

/// The models of the types in `scope` and the reactions between them,
/// tracking the string variables in `also`, the id fields references use
/// (when a reference finds a record by id), and what a reaction copies into
/// a tracked variable, until nothing more reaches one.
pub(super) fn tracked(
    by_name: &BTreeMap<&str, &Automaton>,
    scope: &BTreeSet<String>,
    mut also: BTreeMap<String, BTreeSet<String>>,
    records: Records,
) -> (BTreeMap<String, TemperModel>, Vec<Reaction>) {
    let reactions = reactions_in(by_name, scope);
    loop {
        let mut models: BTreeMap<String, TemperModel> = scope
            .iter()
            .map(|entity| {
                let extra = also.get(entity).cloned().unwrap_or_default();
                (
                    entity.clone(),
                    build_model_tracking(by_name[entity.as_str()], JOINT_MAX_COUNTER, &extra),
                )
            })
            .collect();
        let mut literals = BTreeSet::new();
        for entity in scope {
            let a = by_name[entity.as_str()];
            for inv in &a.invariants {
                collect_literals(&inv.assert, &mut literals);
            }
            for live in &a.liveness {
                if let Some(goal) = &live.eventually {
                    collect_literals(goal, &mut literals);
                }
            }
            for action in &a.actions {
                collect_literals(&action.guard, &mut literals);
            }
        }
        for model in models.values() {
            literals.extend(model.initial_strings.values().filter_map(|v| match v {
                crate::model::ModelString::Literal(s) => Some(s.clone()),
                _ => None,
            }));
        }
        for reaction in &reactions {
            for arg in reaction.args.values() {
                if let Arg::Lit(temper_spec::predicate::Literal::Str(s)) = arg {
                    literals.insert(s.clone());
                }
            }
        }
        for model in models.values_mut() {
            for field in &model.tracked_strings {
                model
                    .string_literals
                    .entry(field.clone())
                    .or_default()
                    .extend(literals.clone());
            }
        }
        let mut grew = false;
        let mut track = |entity: &str, field: &str| {
            !matches!(field, "Id" | "id")
                && also
                    .entry(entity.to_string())
                    .or_default()
                    .insert(field.to_string())
        };
        if records == Records::ById {
            for (entity, model) in &models {
                for t in &model.transitions {
                    for (related, id_field) in t.guard.related_types() {
                        if scope.contains(&related) {
                            grew |= track(entity, &id_field);
                        }
                    }
                }
            }
        }
        for reaction in &reactions {
            // A trigger's guard is decided on its sender's fields.
            if let Some(guard) = &reaction.guard {
                let from = &models[&reaction.from];
                guard.for_each_name(&mut |name| {
                    if let Name::Var(field) = name
                        && from.var_kinds.get(field) != Some(&VarKind::Str)
                        && !from.holds(field)
                    {
                        grew |= track(&reaction.from, field);
                    }
                });
            }
            if let Some(TargetResolver::CreateIfMissing { id_field }) = &reaction.resolve {
                grew |= track(&reaction.from, id_field);
            }
            if records == Records::ById
                && let Some(TargetResolver::Field { id_field }) = &reaction.resolve
            {
                grew |= track(&reaction.from, id_field);
            }
            for (param, arg) in &reaction.args {
                let Arg::Var(field) = arg else { continue };
                let from = &models[&reaction.from];
                // A string is passed only where the target models it; any
                // other field the sender holds only when something reads it.
                let needed = if from.var_kinds.get(field) == Some(&VarKind::Str) {
                    models[&reaction.to].models_string(&reaction.target_action, param)
                        && !from.tracked_strings.contains(field)
                } else {
                    !from.holds(field)
                };
                if needed {
                    grew |= track(&reaction.from, field);
                }
            }
        }
        if !grew {
            return (models, reactions);
        }
    }
}

/// A module call an action's trigger starts.
#[derive(Debug, Clone)]
pub(super) struct CallSpec {
    pub(super) module: String,
    pub(super) on_success: Vec<String>,
    pub(super) on_failure: Option<String>,
}

/// A state timer: in `state`, it fires `action`; `reset_on` arms it again.
#[derive(Debug, Clone)]
pub(in crate::composite::joint) struct Timeout {
    pub(in crate::composite::joint) state: String,
    pub(in crate::composite::joint) action: String,
    pub(in crate::composite::joint) reset_on: Vec<String>,
}

/// What the kernel sends, read once from the specs.
#[derive(Debug, Default)]
pub(super) struct Kernel {
    /// Each entity's state timers.
    pub(super) timers: BTreeMap<String, Vec<Timeout>>,
    /// The module calls each (entity, action) starts.
    pub(super) calls_on: BTreeMap<(String, String), Vec<CallSpec>>,
    /// Each entity's internal actions: what a module whose trigger declares
    /// no `on_success` may answer with.
    pub(super) internal: BTreeMap<String, BTreeSet<String>>,
}

impl Kernel {
    /// Whether the kernel sends `entity`'s `action` with parameters of its
    /// own choosing or none: a module's answer or failure, or a timer.
    pub(super) fn chooses(&self, entity: &str, action: &str) -> bool {
        let named = |name: &Option<String>| name.as_deref() == Some(action);
        self.internal
            .get(entity)
            .is_some_and(|names| names.contains(action))
            || self
                .timers
                .get(entity)
                .is_some_and(|timers| timers.iter().any(|t| t.action == action))
            || self.calls_on.iter().any(|((e, _), specs)| {
                e == entity
                    && specs.iter().any(|spec| {
                        spec.on_success.iter().any(|name| name == action) || named(&spec.on_failure)
                    })
            })
    }

    /// The timers and module calls of the types in `scope`.
    pub(super) fn read(by_name: &BTreeMap<&str, &Automaton>, scope: &BTreeSet<String>) -> Self {
        let mut kernel = Kernel::default();
        for entity in scope {
            let automaton = by_name[entity.as_str()];
            for action in &automaton.actions {
                for trigger in &action.triggers {
                    let (TriggerKind::Wasm, Some(module)) = (&trigger.kind, &trigger.module) else {
                        continue;
                    };
                    kernel
                        .calls_on
                        .entry((entity.clone(), action.name.clone()))
                        .or_default()
                        .push(CallSpec {
                            module: module.clone(),
                            on_success: trigger.on_success.clone(),
                            on_failure: trigger.on_failure.clone(),
                        });
                }
            }
            kernel.internal.insert(
                entity.clone(),
                automaton
                    .actions
                    .iter()
                    .filter(|a| a.kind == ActionKind::Internal)
                    .map(|a| a.name.clone())
                    .collect(),
            );
            kernel.timers.insert(
                entity.clone(),
                automaton
                    .state_timeouts
                    .iter()
                    .map(|timeout| Timeout {
                        state: timeout.state.clone(),
                        action: timeout.on_timeout.clone(),
                        reset_on: timeout.reset_on.clone(),
                    })
                    .collect(),
            );
        }
        kernel
    }
}

/// Each action of a type in `scope` that a reaction from a type outside it
/// sends, with those types: the model, made of the types in `scope`, never
/// sends it.
pub(super) fn reactions_into(
    by_name: &BTreeMap<&str, &Automaton>,
    scope: &BTreeSet<String>,
) -> BTreeMap<(String, String), BTreeSet<String>> {
    let mut into: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    for (from, automaton) in by_name.iter().filter(|(name, _)| !scope.contains(**name)) {
        for trigger in automaton.actions.iter().flat_map(|a| &a.triggers) {
            if let (TriggerKind::Entity, Some(to), Some(action)) = (
                &trigger.kind,
                &trigger.target_entity,
                &trigger.target_action,
            ) && scope.contains(to)
            {
                into.entry((to.clone(), action.clone()))
                    .or_default()
                    .insert(from.to_string());
            }
        }
    }
    into
}

/// The entity reactions whose source and target are both in `scope`.
fn reactions_in(by_name: &BTreeMap<&str, &Automaton>, scope: &BTreeSet<String>) -> Vec<Reaction> {
    let mut reactions = Vec::new();
    for from in scope {
        for action in &by_name[from.as_str()].actions {
            for trigger in &action.triggers {
                if trigger.kind != TriggerKind::Entity {
                    continue;
                }
                let (Some(to), Some(target_action)) =
                    (&trigger.target_entity, &trigger.target_action)
                else {
                    continue;
                };
                if !scope.contains(to) {
                    continue;
                }
                reactions.push(Reaction {
                    from: from.clone(),
                    source_action: action.name.clone(),
                    to: to.clone(),
                    target_action: target_action.clone(),
                    guard: trigger.guard.clone(),
                    args: trigger.args.clone(),
                    resolve: trigger.resolve_target.clone(),
                    principal: senders::declared(trigger, from, &action.name),
                    trigger: trigger.name.clone(),
                    may_drop: trigger.drop_ok
                        || matches!(trigger.resolve_target, Some(TargetResolver::Create)),
                });
            }
        }
    }
    reactions
}

/// The actions (entity, action) with a constraint the model cannot decide,
/// and what a module call keeps to decide its answer's.
#[derive(Debug, Default)]
pub(super) struct Unchecked {
    /// Sent as a dispatch or a request: one on a counter the order does not
    /// keep, or on a string parameter or field the model gives no value.
    pub(super) taken: BTreeSet<(String, String)>,
    /// Sent with parameters its sender chooses (a module's answer, or a
    /// request from outside): one that ties a parameter to a field the model
    /// does not track. Any other constraint the sender can meet. A module
    /// answers about the record as it was when the call started, so a call
    /// keeps the tracked fields its answers are tied to (`asked`).
    pub(super) chosen: BTreeSet<(String, String)>,
    /// Each (entity, action)'s tracked fields a parameter must equal.
    pub(super) equals: BTreeMap<(String, String), BTreeSet<String>>,
    /// Each (entity, action)'s count parameters that must equal a counter
    /// the model holds as a value (not as a sequence number), with that
    /// counter. Decided when a dispatch carries the count, and only then.
    pub(super) counts: BTreeMap<(String, String), Vec<(String, String)>>,
    /// Each (entity, action)'s string parameters that must not be empty and
    /// that the model gives no value. Decided when a dispatch carries the
    /// value, and only then.
    pub(super) nonempty: BTreeMap<(String, String), Vec<String>>,
}

impl Unchecked {
    /// Whether the model decides every constraint of `entity`'s `action`,
    /// sent as a dispatch or a request.
    pub(super) fn decides(&self, entity: &str, action: &str) -> bool {
        !self
            .taken
            .contains(&(entity.to_string(), action.to_string()))
    }

    /// Whether a sender choosing the parameters of `entity`'s `action` can
    /// meet its constraints, as far as the model knows.
    pub(super) fn meets(&self, entity: &str, action: &str) -> bool {
        !self
            .chosen
            .contains(&(entity.to_string(), action.to_string()))
    }

    /// The count parameters of `entity`'s `action` that must equal one of
    /// its counters, each with the counter.
    pub(super) fn counts(&self, entity: &str, action: &str) -> &[(String, String)] {
        self.counts
            .get(&(entity.to_string(), action.to_string()))
            .map_or(&[], Vec::as_slice)
    }

    /// The string parameters of `entity`'s `action` the model gives no value
    /// that must not be empty.
    pub(super) fn nonempty(&self, entity: &str, action: &str) -> &[String] {
        self.nonempty
            .get(&(entity.to_string(), action.to_string()))
            .map_or(&[], Vec::as_slice)
    }

    /// The fields an `entity` record's module call keeps as it started:
    /// those the actions it may answer with (`answers`) tie a parameter to.
    pub(super) fn asked(&self, entity: &str, answers: impl Fn(&str) -> bool) -> BTreeSet<String> {
        self.equals
            .iter()
            .filter(|((e, action), _)| e == entity && answers(action))
            .flat_map(|(_, fields)| fields.iter().cloned())
            .collect()
    }
}

/// What the model cannot decide of each action's constraints. Taking such
/// an action as allowed only adds behaviour for an invariant; for liveness it
/// is one of allowed and refused.
pub(super) fn unchecked_constraints(
    by_name: &BTreeMap<&str, &Automaton>,
    models: &BTreeMap<String, TemperModel>,
    order: &Order,
) -> Unchecked {
    let mut out = Unchecked::default();
    for (entity, model) in models {
        let tracked = |name: &str| model.tracked_strings.contains(name);
        let counted = |name: &str| {
            model.var_kinds.get(name) == Some(&VarKind::Counter)
                && !order.fields(entity).any(|(field, _)| field == name)
        };
        for action in &by_name[entity.as_str()].actions {
            let key = (entity.clone(), action.name.clone());
            let modelled = |param: &str| model.models_string(&action.name, param);
            let decided = |constraint: &ActionConstraint| match constraint {
                ActionConstraint::ParamGreaterThanField { param, field } => {
                    order.decides(entity, &action.name, param, field)
                }
                ActionConstraint::ParamEqualsField { field, .. } if counted(field) => true,
                ActionConstraint::ParamNonempty { .. } => true,
                _ => modelled(constraint.param()) && constraint.field().is_none_or(tracked),
            };
            if !action.constraints.iter().all(decided) {
                out.taken.insert(key.clone());
            }
            for constraint in &action.constraints {
                if let ActionConstraint::ParamNonempty { param } = constraint
                    && !modelled(param)
                {
                    out.nonempty
                        .entry(key.clone())
                        .or_default()
                        .push(param.clone());
                }
                let ActionConstraint::ParamEqualsField { param, field } = constraint else {
                    continue;
                };
                if counted(field) {
                    out.counts
                        .entry(key.clone())
                        .or_default()
                        .push((param.clone(), field.clone()));
                }
                if tracked(field) {
                    out.equals
                        .entry(key.clone())
                        .or_default()
                        .insert(field.clone());
                } else {
                    out.chosen.insert(key.clone());
                }
            }
        }
    }
    out
}

/// The string literals the properties (`reads`, about `owner`) and the
/// guards in scope compare a field with, by (entity, field). The model offers
/// them as values a parameter may carry: a value only another spec names is
/// still one a request may send.
pub(super) fn named_literals(
    owner: &str,
    reads: &[&Expr],
    models: &BTreeMap<String, TemperModel>,
    reactions: &[Reaction],
) -> BTreeMap<(String, String), BTreeSet<String>> {
    let mut out: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    let exprs = reads
        .iter()
        .map(|expr| (owner, *expr))
        .chain(models.iter().flat_map(|(entity, model)| {
            model
                .transitions
                .iter()
                .map(move |t| (entity.as_str(), &t.guard))
        }))
        .chain(
            reactions
                .iter()
                .filter_map(|r| Some((r.from.as_str(), r.guard.as_ref()?))),
        );
    for (entity, expr) in exprs {
        literals_in(entity, expr, &mut out);
    }
    out
}

fn literals_in(entity: &str, expr: &Expr, out: &mut BTreeMap<(String, String), BTreeSet<String>>) {
    let field = |operand: &Operand| match operand {
        Operand::Var(var) => Some((entity.to_string(), var.clone())),
        Operand::CrossField {
            entity_type, field, ..
        } => Some((entity_type.clone(), field.clone())),
        _ => None,
    };
    match expr {
        Expr::Not(inner) => literals_in(entity, inner, out),
        Expr::And(parts) | Expr::Or(parts) => {
            parts.iter().for_each(|part| literals_in(entity, part, out));
        }
        Expr::Implies(a, b) => {
            literals_in(entity, a, out);
            literals_in(entity, b, out);
        }
        Expr::Compare {
            lhs,
            op: CmpOp::Eq | CmpOp::Ne,
            rhs,
        } => {
            for (side, other) in [(lhs, rhs), (rhs, lhs)] {
                if let (Some(key), Operand::Lit(Literal::Str(text))) = (field(side), other) {
                    out.entry(key).or_default().insert(text.clone());
                }
            }
        }
        Expr::In {
            value,
            set: Set::List(items),
            ..
        } => {
            if let Some(key) = field(value) {
                for item in items {
                    if let Literal::Str(text) = item {
                        out.entry(key.clone()).or_default().insert(text.clone());
                    }
                }
            }
        }
        _ => {}
    }
}

/// Literals must remain distinct even when compared with related operands.
pub(super) fn collect_literals(expr: &Expr, out: &mut BTreeSet<String>) {
    use temper_spec::predicate::{Literal, Operand, Set};
    let mut operand = |o: &Operand| {
        if let Operand::Lit(Literal::Str(s)) = o {
            out.insert(s.clone());
        }
    };
    match expr {
        Expr::Compare { lhs, rhs, .. } => {
            operand(lhs);
            operand(rhs);
        }
        Expr::In {
            value,
            set: Set::List(items),
            ..
        } => {
            operand(value);
            for item in items {
                if let Literal::Str(s) = item {
                    out.insert(s.clone());
                }
            }
        }
        Expr::Not(e) => collect_literals(e, out),
        Expr::And(parts) | Expr::Or(parts) => {
            for e in parts {
                collect_literals(e, out);
            }
        }
        Expr::Implies(a, b) => {
            collect_literals(a, out);
            collect_literals(b, out);
        }
        _ => {}
    }
}
