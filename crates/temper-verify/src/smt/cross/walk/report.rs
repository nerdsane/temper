//! Asking the rule at the end of a path, and writing down a break.

use std::collections::BTreeSet;

use z3::ast::Int;

use super::{Step, Walk};
use crate::smt::cross::CrossBreak;
use crate::smt::cross::world::{Atoms, World};

impl<'a> Walk<'a> {
    /// Nothing is in flight: does any record of the owner's type break the
    /// rule now?
    pub(super) fn ask(&mut self, world: &World, path: &[Step]) {
        for i in world.of(self.owner) {
            let rec = world.records[i].clone();
            let holds = self.read(&rec, world, None, self.rule);
            self.solver.push();
            self.solver.assert(&rec.exists);
            self.solver.assert(holds.not());
            if self.sat() {
                self.note_break(world, i, path);
            }
            self.solver.pop(1);
        }
    }

    pub(super) fn note_break(&mut self, world: &World, broken: usize, path: &[Step]) {
        let moves: Vec<&str> = path.iter().map(|s| s.key.as_str()).collect();
        let key = moves.join(" > ");
        let at = path
            .iter()
            .rev()
            .map(|s| s.key.as_str())
            .find(|key| {
                self.breakers.iter().any(|(ty, name)| {
                    key.strip_suffix(name.as_str())
                        .and_then(|k| k.strip_suffix('.'))
                        == Some(ty.as_str())
                })
            })
            .unwrap_or(moves.last().copied().unwrap_or(""))
            .to_string();
        let from_start = !key.starts_with('~');
        if let Some(wanted) = &mut self.wanted {
            if !from_start || !wanted.remove(&at) {
                return;
            }
            self.stop = wanted.is_empty();
        } else {
            self.stop = true;
        }
        let group = format!("{at} {from_start}");
        if self
            .breaks
            .get(&group)
            .is_some_and(|b| b.key.len() <= key.len())
        {
            return;
        }
        let Some(model) = self.solver.get_model() else {
            return;
        };
        let show = |atoms: &Atoms, value: &Int| {
            model
                .eval(value, true)
                .and_then(|v| v.as_i64())
                .map_or_else(|| "?".to_string(), |v| atoms.show(v))
        };
        let steps = path
            .iter()
            .map(|step| {
                let shown: Vec<String> = step
                    .shown
                    .iter()
                    .map(|(k, v)| format!("{k}={}", show(&self.atoms, v)))
                    .collect();
                format!("{} [{}]", step.text, shown.join(", "))
            })
            .collect();
        let mut rule_fields = BTreeSet::new();
        crate::smt::cross::scope::own_reads(self.rule, &mut rule_fields);
        let records = world
            .records
            .iter()
            .zip(&self.before.records)
            .filter(|(after, _)| {
                self.scope.types.contains(&after.ty) || after.ty == world.records[broken].ty
            })
            .map(|(after, before)| {
                let fields: Vec<String> = after
                    .fields
                    .iter()
                    .filter(|(name, _)| rule_fields.contains(*name))
                    .map(|(name, value)| {
                        let old = before.fields.get(name).unwrap_or(value);
                        format!(
                            "{name}: {} -> {}",
                            show(&self.atoms, old),
                            show(&self.atoms, value)
                        )
                    })
                    .collect();
                let made = model.eval(&before.exists, true).and_then(|v| v.as_bool());
                format!(
                    "{} id={} {}{} -> {} {}",
                    after.name,
                    show(&self.atoms, &after.id),
                    if made == Some(false) {
                        "(not made) "
                    } else {
                        ""
                    },
                    show(&self.atoms, &before.status),
                    show(&self.atoms, &after.status),
                    fields.join(", ")
                )
            })
            .collect();
        self.breaks.insert(
            group,
            CrossBreak {
                from_start,
                at,
                key,
                record: world.records[broken].name.clone(),
                steps,
                records,
            },
        );
    }
}
