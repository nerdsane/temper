//! What the joint model reads once from the specs: each type's model with
//! the strings it must track, the reactions between the types in scope, the
//! module calls their actions start, their internal actions and state
//! timers, and the constraints it does not check.

mod constraints;

use std::collections::{BTreeMap, BTreeSet};

use temper_authz::Asker;
use temper_spec::automaton::{
    ActionConstraint, ActionKind, Automaton, TargetResolver, TriggerKind,
};
use temper_spec::predicate::{Arg, CmpOp, Expr, Literal, Name, Operand, Set, VarKind};

use super::super::env::Records;
use super::super::senders;
use super::JOINT_MAX_COUNTER;
use super::classes::Classes;
use crate::model::{TemperModel, build_model_tracking};

pub(super) use constraints::{Unchecked, unchecked_constraints};

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
    /// What it does as a spawn, when it stands for one.
    pub(super) spawn: Option<super::spawn::Spawn>,
}

/// Whether `automaton` refuses `action` with `param` empty.
fn requires(automaton: &Automaton, action: &str, param: &str) -> bool {
    automaton
        .actions
        .iter()
        .filter(|a| a.name == action)
        .flat_map(|a| &a.constraints)
        .any(|c| matches!(c, ActionConstraint::ParamNonempty { param: p } if p == param))
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
    let classes = Classes::read(by_name, scope);
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
        for (entity, model) in &mut models {
            for field in &model.tracked_strings {
                model
                    .string_literals
                    .entry(field.clone())
                    .or_default()
                    .extend(classes.literals(entity, field));
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
            // A trigger's guard may find a record by a field of its sender,
            // and the walk can read that record only from the field's value.
            if records == Records::ById
                && let Some(guard) = &reaction.guard
            {
                for (related, id_field) in guard.related_types() {
                    if scope.contains(&related) {
                        grew |= track(&reaction.from, &id_field);
                    }
                }
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
                // A string is passed only where the target models it or
                // refuses a move without it; any other field the sender holds
                // only when something reads it.
                let needed = if from.var_kinds.get(field) == Some(&VarKind::Str) {
                    (models[&reaction.to].models_string(&reaction.target_action, param)
                        || requires(
                            by_name[reaction.to.as_str()],
                            &reaction.target_action,
                            param,
                        ))
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
        for (to, action) in automaton
            .actions
            .iter()
            .flat_map(super::spawn::spawns_of)
            .map(|(to, action, ..)| (to, action))
            .filter(|(to, _)| scope.contains(*to))
        {
            into.entry((to.clone(), action.clone()))
                .or_default()
                .insert(from.to_string());
        }
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
            // Before its triggers: a commit stores a child's id before any
            // of its trigger guards is read.
            reactions.extend(super::spawn::spawn_reactions(by_name, scope, from, action));
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
                    guard: trigger.guard.as_ref().map(|guard| {
                        crate::model::trigger_guard::as_read(by_name[from.as_str()], action, guard)
                    }),
                    args: trigger.args.clone(),
                    resolve: trigger.resolve_target.clone(),
                    principal: senders::declared(trigger, from, &action.name),
                    trigger: trigger.name.clone(),
                    may_drop: trigger.drop_ok
                        || matches!(trigger.resolve_target, Some(TargetResolver::Create)),
                    spawn: None,
                });
            }
        }
    }
    reactions
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

/// The fields of an `entity` record that decide whether a move happens
/// without being a guard: those a constraint compares a parameter with, and
/// those the guard of a trigger it fires reads. A cut keeps them, so a
/// record refusing a dispatch for a stale count, or a reaction it does not
/// send, stays decided.
pub(super) fn deciding(
    automaton: &Automaton,
    reactions: &[Reaction],
    entity: &str,
) -> BTreeSet<String> {
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
