//! Which text literals each record id and text field can meet.
//!
//! Two terms (a type's field, id or status) share a class when anything in
//! scope can tell their values apart or carry one into the other: a guard,
//! rule, trigger guard or liveness goal compares them (`==`, `!=`, `<`,
//! `in` a list field), an effect, a reaction's or a timer's arguments, or a
//! spawn copies one into the other, a constraint ties a parameter to a
//! field, a related read `T[f].x` or a reaction's target reference finds a
//! record by its id, or a same-id reaction gives two types one id. A move's
//! parameter counts as the field of its name, which the runtime writes.
//!
//! A class holds the literals its terms are compared with, start with, are
//! assigned or have appended, and are sent as a reaction's, timer's or
//! spawn's literal arguments or a static target's id; a status class holds
//! its type's places. The walk offers each id and tracked field only its own
//! class's literals. A literal no term of a class is compared with or
//! receives is, to everything that reads that class, a value like any made
//! up one, and the walk already offers made-up values (`#n`, blank or not)
//! everywhere: a state that holds such a literal is a renaming of one the
//! walk still reaches, and every guard, rule and liveness verdict is the
//! same on both.
//!
//! Literals of one class are one value to the walk when every read of the
//! class treats them alike. A read that compares the class with one literal,
//! or with a list, tells apart only the literals it names; a reaction's or
//! effect's copying tells nothing, and neither does `empty`. So two literals
//! named by exactly the same reads give every guard, rule and liveness goal
//! the same answer, and the walk offers one of them
//! ([`Classes::one_of_each_kind`]). A class whose value itself is read keeps
//! all its literals: it holds an id, which finds a record, or a status, or a
//! list, which counts what it holds, or two of its terms are compared.

use std::collections::{BTreeMap, BTreeSet};

use temper_spec::automaton::{Automaton, Initial, TargetResolver, TriggerKind};
use temper_spec::predicate::{Arg, CmpOp, Effect, Expr, Literal, Operand, Set};

use crate::model::TemperModel;

/// What in a record a value sits in.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Slot {
    Id,
    Status,
    Field(String),
}

impl Slot {
    fn named(name: &str) -> Self {
        if matches!(name, "Id" | "id") {
            Slot::Id
        } else {
            Slot::Field(name.to_string())
        }
    }
}

/// A slot of one entity type.
type Term = (String, Slot);

/// The equality classes of the terms of the types in a scope, each with the
/// literals it holds.
#[derive(Debug, Default)]
pub(in crate::composite::joint) struct Classes {
    parent: BTreeMap<Term, Term>,
    held: BTreeMap<Term, BTreeSet<String>>,
    /// The literals each read that names some of them names.
    reads: Vec<(Term, BTreeSet<String>)>,
    /// Terms whose value itself is read: their classes keep every literal.
    exact: Vec<Term>,
}

impl Classes {
    /// The classes of everything the types in `scope` declare.
    pub(in crate::composite::joint) fn read(
        by_name: &BTreeMap<&str, &Automaton>,
        scope: &BTreeSet<String>,
    ) -> Self {
        let mut classes = Classes::default();
        for entity in scope {
            classes.declare(entity, by_name[entity.as_str()]);
        }
        classes
    }

    /// The literals `entity`'s field `field` (or its id) can meet, `''` left
    /// out: the walk always offers it.
    pub(in crate::composite::joint) fn literals(
        &self,
        entity: &str,
        field: &str,
    ) -> BTreeSet<String> {
        let root = self.root(&(entity.to_string(), Slot::named(field)));
        let mut literals = self.held.get(&root).cloned().unwrap_or_default();
        literals.remove("");
        literals
    }

    /// Leave each field and parameter of `models` only the literals something
    /// that reads it compares or copies there. A value Cedar reads (`cedar`)
    /// is told apart by whatever the policy says of it.
    pub(in crate::composite::joint) fn narrow(
        &self,
        models: &mut BTreeMap<String, TemperModel>,
        cedar: impl Fn(&str) -> bool,
    ) {
        for (entity, model) in models {
            let parameters = model.transitions.iter().flat_map(|t| &t.string_params);
            let names: BTreeSet<String> = parameters
                .map(|param| param.name.clone())
                .chain(model.tracked_strings.iter().cloned())
                .collect();
            for name in names {
                let literals = model.string_literals.entry(name.clone()).or_default();
                literals.extend(self.literals(entity, &name));
                if !cedar(&name) {
                    *literals = self.one_of_each_kind(entity, &name, literals);
                }
            }
            model.literals_by_class = true;
        }
    }

    /// `literals`, which `entity`'s field `field` may be offered, without
    /// those its class doesn't hold and with one of each kind the reads of
    /// its class can tell apart. `''` stays.
    pub(in crate::composite::joint) fn one_of_each_kind(
        &self,
        entity: &str,
        field: &str,
        literals: &BTreeSet<String>,
    ) -> BTreeSet<String> {
        let root = self.root(&(entity.to_string(), Slot::named(field)));
        // A model offers its inputs the literals of each other's classes.
        let none = BTreeSet::new();
        let held = self.held.get(&root).unwrap_or(&none);
        let literals = literals
            .iter()
            .filter(|literal| literal.is_empty() || held.contains(*literal));
        if self.exact.iter().any(|term| self.root(term) == root) {
            return literals.cloned().collect();
        }
        let reads: Vec<&BTreeSet<String>> = self
            .reads
            .iter()
            .filter(|(term, _)| self.root(term) == root)
            .map(|(_, named)| named)
            .collect();
        let mut kinds = BTreeSet::new();
        literals
            .filter(|literal| {
                let kind: Vec<bool> = reads.iter().map(|named| named.contains(*literal)).collect();
                literal.is_empty() || kinds.insert(kind)
            })
            .cloned()
            .collect()
    }

    fn declare(&mut self, entity: &str, a: &Automaton) {
        let term = |name: &str| (entity.to_string(), Slot::named(name));
        let own = (entity.to_string(), Slot::Status);
        self.hold_all(&own, a.automaton.states.iter().cloned());
        self.exact.extend([own, term("Id")]);
        for sv in &a.state {
            match &sv.initial {
                Initial::String(value) => self.hold(&term(&sv.name), value),
                Initial::List(items) => {
                    self.exact.push(term(&sv.name));
                    self.hold_all(&term(&sv.name), items.iter().cloned());
                }
                _ => {}
            }
        }
        for inv in &a.invariants {
            self.visit(entity, &inv.assert);
        }
        for rule in &a.entry_rules {
            self.visit(entity, &rule.assert);
        }
        for goal in a
            .liveness
            .iter()
            .filter_map(|live| live.eventually.as_ref())
        {
            self.visit(entity, goal);
        }
        for timeout in &a.state_timeouts {
            for (param, arg) in &timeout.args {
                self.copy(&term(param), entity, arg);
            }
        }
        for action in &a.actions {
            self.visit(entity, &action.guard);
            for constraint in &action.constraints {
                if let Some(field) = constraint.field() {
                    self.link(&term(constraint.param()), &term(field));
                    self.exact.push(term(field));
                }
            }
            for effect in &action.effect {
                match effect {
                    Effect::Assign { var, value, .. } => self.copy(&term(var), entity, value),
                    Effect::Append { list, value } | Effect::Remove { list, value } => {
                        self.exact.push(term(list));
                        self.copy(&term(list), entity, value);
                    }
                    Effect::Spawn {
                        entity_type,
                        store_id_in,
                        id,
                        copy,
                        ..
                    } => {
                        let child = |name: &str| (entity_type.clone(), Slot::named(name));
                        self.exact.push(child("Id"));
                        if let Some(field) = store_id_in {
                            self.link(&term(field), &child("Id"));
                        }
                        if let Some(id) = id {
                            self.copy(&child("Id"), entity, id);
                        }
                        // The child's first action gets this action's
                        // parameters and the copied fields.
                        let passed = action
                            .params
                            .iter()
                            .map(|p| p.name())
                            .chain(copy.iter().map(String::as_str));
                        for name in passed {
                            self.link(&term(name), &child(name));
                        }
                    }
                    _ => {}
                }
            }
            for trigger in &action.triggers {
                if let Some(guard) = &trigger.guard {
                    self.visit(entity, guard);
                }
                let (TriggerKind::Entity, Some(to)) = (&trigger.kind, &trigger.target_entity)
                else {
                    continue;
                };
                let target = |name: &str| (to.clone(), Slot::named(name));
                self.exact.push(target("Id"));
                for (param, arg) in &trigger.args {
                    self.copy(&target(param), entity, arg);
                }
                match &trigger.resolve_target {
                    Some(
                        TargetResolver::Field { id_field }
                        | TargetResolver::CreateIfMissing { id_field },
                    ) => self.link(&term(id_field), &target("Id")),
                    Some(TargetResolver::SameId) => self.link(&term("Id"), &target("Id")),
                    Some(TargetResolver::Static { entity_id }) => {
                        self.hold(&target("Id"), entity_id);
                    }
                    Some(TargetResolver::Create) | None => {}
                }
            }
        }
    }

    /// `value`, written by an `entity` move, goes into `into`.
    fn copy(&mut self, into: &Term, entity: &str, value: &Arg) {
        match value {
            Arg::Lit(Literal::Str(text)) => self.hold(into, text),
            Arg::Var(name) | Arg::Param(name) => {
                self.link(into, &(entity.to_string(), Slot::named(name)));
            }
            Arg::Lit(_) => {}
        }
    }

    /// The term `operand` of an `owner` predicate reads, linking the id
    /// field a related read finds its record by with that record's id.
    fn term(&mut self, owner: &str, operand: &Operand) -> Option<Term> {
        let mut related = |entity_type: &str, id_field: &str, slot: Slot| {
            let id = (entity_type.to_string(), Slot::Id);
            self.exact.push(id.clone());
            self.link(&(owner.to_string(), Slot::named(id_field)), &id);
            Some((entity_type.to_string(), slot))
        };
        match operand {
            Operand::Var(name) | Operand::Param(name) => {
                Some((owner.to_string(), Slot::named(name)))
            }
            Operand::Status => Some((owner.to_string(), Slot::Status)),
            Operand::CrossStatus {
                entity_type,
                id_field,
            } => related(entity_type, id_field, Slot::Status),
            Operand::CrossField {
                entity_type,
                id_field,
                field,
            } => related(entity_type, id_field, Slot::named(field)),
            Operand::Len(_) | Operand::Lit(_) => None,
        }
    }

    fn visit(&mut self, owner: &str, expr: &Expr) {
        match expr {
            Expr::Not(inner) => self.visit(owner, inner),
            Expr::And(parts) | Expr::Or(parts) => {
                parts.iter().for_each(|part| self.visit(owner, part));
            }
            Expr::Implies(a, b) => {
                self.visit(owner, a);
                self.visit(owner, b);
            }
            Expr::Compare { lhs, op, rhs } => {
                match (self.term(owner, lhs), self.term(owner, rhs), lhs, rhs) {
                    (Some(a), Some(b), _, _) => {
                        self.exact.push(a.clone());
                        self.link(&a, &b);
                    }
                    (Some(t), None, _, Operand::Lit(Literal::Str(text)))
                    | (None, Some(t), Operand::Lit(Literal::Str(text)), _) => {
                        self.hold(&t, text);
                        if matches!(op, CmpOp::Eq | CmpOp::Ne) {
                            self.reads.push((t, BTreeSet::from([text.clone()])));
                        } else {
                            self.exact.push(t);
                        }
                    }
                    _ => {}
                }
            }
            Expr::In { value, set, .. } => {
                let value_term = self.term(owner, value);
                match (value_term, set, value) {
                    (Some(t), Set::List(items), _) => {
                        let mut named = BTreeSet::new();
                        for item in items {
                            if let Literal::Str(text) = item {
                                self.hold(&t, text);
                                named.insert(text.clone());
                            }
                        }
                        self.reads.push((t, named));
                    }
                    (Some(t), Set::Var(list), _) => {
                        self.exact.push(t.clone());
                        self.link(&t, &(owner.to_string(), Slot::named(list)));
                    }
                    (None, Set::Var(list), Operand::Lit(Literal::Str(text))) => {
                        let list = (owner.to_string(), Slot::named(list));
                        self.exact.push(list.clone());
                        self.hold(&list, text);
                    }
                    _ => {}
                }
            }
            Expr::Const(_) | Expr::Empty(_) | Expr::Var(_) => {}
        }
    }

    fn root(&self, term: &Term) -> Term {
        let mut at = term;
        while let Some(up) = self.parent.get(at) {
            at = up;
        }
        at.clone()
    }

    fn hold(&mut self, term: &Term, literal: &str) {
        let root = self.root(term);
        self.held
            .entry(root)
            .or_default()
            .insert(literal.to_string());
    }

    fn hold_all(&mut self, term: &Term, literals: impl IntoIterator<Item = String>) {
        let root = self.root(term);
        self.held.entry(root).or_default().extend(literals);
    }

    fn link(&mut self, a: &Term, b: &Term) {
        let (a, b) = (self.root(a), self.root(b));
        if a == b {
            return;
        }
        let moved = self.held.remove(&a).unwrap_or_default();
        self.held.entry(b.clone()).or_default().extend(moved);
        self.parent.insert(a, b);
    }
}
