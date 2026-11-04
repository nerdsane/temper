//! What one move may do and what it writes.

use std::collections::BTreeMap;

use temper_spec::automaton::{Action, ActionConstraint, Effect, VarType};
use temper_spec::predicate::{Arg, AssignOp};
use z3::ast::{Bool, Int};

use super::Walk;
use crate::smt::cross::world::{ABSENT, EMPTY, Rec, World};

impl<'a> Walk<'a> {
    /// Whether `action` can be taken on `rec` with `params`.
    pub(super) fn enabled(
        &mut self,
        rec: &Rec,
        world: &World,
        action: &Action,
        params: &BTreeMap<String, Int>,
    ) -> Bool {
        let mut parts = Vec::new();
        if !action.from.is_empty() {
            let from: Vec<Bool> = action
                .from
                .iter()
                .map(|s| rec.status.eq(Int::from_i64(self.atoms.atom(s))))
                .collect();
            parts.push(Bool::or(&from));
        }
        parts.push(self.read(rec, world, Some(params), &action.guard));
        let param = |name: &str| {
            params
                .get(name)
                .cloned()
                .unwrap_or_else(|| Int::from_i64(ABSENT))
        };
        for constraint in &action.constraints {
            parts.push(match constraint {
                ActionConstraint::ParamEqualsField { param: p, field } => {
                    param(p).eq(rec.field(field))
                }
                ActionConstraint::ParamNotEqualsField { param: p, field } => {
                    param(p).eq(rec.field(field)).not()
                }
                ActionConstraint::ParamGreaterThanField { param: p, field } => {
                    param(p).gt(rec.field(field))
                }
                ActionConstraint::ParamNonempty { param: p } => Bool::and(&[
                    param(p).eq(Int::from_i64(EMPTY)).not(),
                    param(p).eq(Int::from_i64(ABSENT)).not(),
                ]),
            });
        }
        Bool::and(&parts)
    }

    /// An id the kernel makes up for a new record of `ty`, on behalf of
    /// `holder`: no record that has been made holds it, and no field that holds
    /// such ids holds it, except on `holder`, which may have sent this a while
    /// ago.
    pub(super) fn minted(&mut self, ty: &str, holder: &Rec) -> Int {
        let new = Int::fresh_const("minted");
        self.solver.assert(new.eq(Int::from_i64(EMPTY)).not());
        self.solver.assert(new.eq(Int::from_i64(ABSENT)).not());
        for i in self.before.of(ty) {
            let rec = &self.before.records[i];
            self.solver
                .assert(rec.exists.implies(new.eq(&rec.id).not()));
        }
        for maker in self.makers.get(ty).cloned().unwrap_or_default() {
            for i in self.before.of(&maker.owner) {
                let rec = &self.before.records[i];
                let Some(held) = rec.fields.get(&maker.field) else {
                    continue;
                };
                let may_hold = if maker.owner == holder.ty {
                    rec.id.eq(&holder.id)
                } else {
                    Bool::from_bool(false)
                };
                self.solver.assert(new.eq(held).implies(may_hold));
            }
        }
        new
    }

    /// A move from outside does not land on a record in its first status whose
    /// id the kernel minted for a spawn. Someone could read the id off its holder
    /// and send that move before the spawn lands; this leaves that out, so a
    /// rule that needs it is for the full walk to check.
    pub(super) fn not_a_minted_id(&mut self, rec: &Rec) {
        let Some(first) = self.spec(&rec.ty).map(|spec| &spec.automaton.initial) else {
            return;
        };
        let first = Int::from_i64(self.atoms.atom(first));
        let unstarted = rec.status.eq(first);
        for maker in self.makers.get(&rec.ty).cloned().unwrap_or_default() {
            if !maker.minted {
                continue;
            }
            for i in self.before.of(&maker.owner) {
                if let Some(held) = self.before.records[i].fields.get(&maker.field) {
                    self.solver.assert(unstarted.implies(rec.id.eq(held).not()));
                }
            }
        }
    }

    pub(super) fn arg_value(
        &mut self,
        rec: &Rec,
        params: &BTreeMap<String, Int>,
        arg: &Arg,
    ) -> Int {
        match arg {
            Arg::Lit(lit) => Int::from_i64(self.atoms.literal(lit)),
            Arg::Var(name) => rec.field(name),
            Arg::Param(name) => params
                .get(name)
                .cloned()
                .unwrap_or_else(|| Int::from_i64(ABSENT)),
        }
    }

    /// `rec` after `action`, and the ids of the children it spawns.
    pub(super) fn apply(
        &mut self,
        rec: &Rec,
        action: &Action,
        params: &BTreeMap<String, Int>,
    ) -> (Rec, Vec<Int>) {
        let spec = self.spec(&rec.ty);
        let mut next = rec.clone();
        next.exists = Bool::from_bool(true);
        if let Some(to) = &action.to {
            next.status = Int::from_i64(self.atoms.atom(to));
        }
        let zero = Int::from_i64(0);
        let mut spawned = Vec::new();
        for effect in &action.effect {
            match effect {
                Effect::Assign { var, op, value } => {
                    let value = self.arg_value(&next, params, value);
                    let old = next.field(var);
                    let new = match op {
                        AssignOp::Set => value,
                        AssignOp::Add => Int::add(&[&old, &value]),
                        AssignOp::Sub => {
                            let less = Int::sub(&[&old, &value]);
                            less.ge(&zero).ite(&less, &zero)
                        }
                    };
                    if next.fields.contains_key(var) {
                        next.fields.insert(var.clone(), new);
                    }
                }
                Effect::Append { list, .. } => {
                    if next.fields.contains_key(list) {
                        let old = next.field(list);
                        next.fields
                            .insert(list.clone(), Int::add(&[&old, &Int::from_i64(1)]));
                    }
                }
                Effect::RemoveAt { list, .. } | Effect::Remove { list, .. } => {
                    if next.fields.contains_key(list) {
                        let left = Int::fresh_const("left");
                        self.solver.assert(left.ge(&zero));
                        next.fields.insert(list.clone(), left);
                    }
                }
                Effect::Spawn {
                    entity_type,
                    store_id_in,
                    id,
                    ..
                } => {
                    let child = match id {
                        Some(arg) => self.arg_value(&next, params, arg),
                        None => self.minted(entity_type, rec),
                    };
                    if let Some(field) = store_id_in
                        && next.fields.contains_key(field)
                    {
                        next.fields.insert(field.clone(), child.clone());
                    }
                    spawned.push(child);
                }
                Effect::Schedule { .. } | Effect::ScheduleAt { .. } => {}
            }
        }
        let strict = spec.is_some_and(|s| s.automaton.strict_action_params);
        for (name, value) in params {
            let declared = action.params.iter().any(|p| p.name() == name);
            if !next.fields.contains_key(name) || (strict && !declared) {
                continue;
            }
            let list = spec.is_some_and(|s| {
                s.state
                    .iter()
                    .any(|v| &v.name == name && v.var_type == VarType::List)
            });
            if list {
                let left = Int::fresh_const("list");
                self.solver.assert(left.ge(&zero));
                next.fields.insert(name.clone(), left);
            } else {
                next.fields.insert(name.clone(), value.clone());
            }
        }
        (next, spawned)
    }
}
