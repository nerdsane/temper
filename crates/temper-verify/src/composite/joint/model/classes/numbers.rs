//! What numbers made-up values by class, and what checks they stay in it.

use std::collections::{BTreeMap, BTreeSet};

use temper_authz::Read;

use super::super::super::JointState;
use super::super::super::env::ID;
use super::super::JointModel;
use super::{Classes, Slot, Term};
use crate::model::{ModelString, ParamValue, TemperModel};

impl JointModel {
    /// A made-up value of `state` outside the class of the field or parameter
    /// holding it, which would mean something copied or compared it that
    /// [`Classes`] did not read. Checked steps look for one.
    pub(in crate::composite::joint) fn value_out_of_class(
        &self,
        state: &JointState,
    ) -> Option<String> {
        let stray = |entity: &str, field: &str, value: &ModelString| {
            let class = self.models.get(entity)?.string_classes.get(field)?;
            value
                .class()
                .is_some_and(|of| of != *class)
                .then(|| format!("{entity}.{field} holds {value} of another class than {class}"))
        };
        let in_records = state.records.iter().flat_map(|(entity, record)| {
            let strings = record.strings.iter().map(|(f, v)| (*f, v));
            let lists = record
                .lists
                .iter()
                .flat_map(|(f, list)| list.iter().map(|v| (*f, v)));
            strings
                .chain(lists)
                .filter_map(move |(f, v)| stray(entity.as_str(), f.as_str(), v))
        });
        let on_their_way = state.inflight.iter().flat_map(|dispatch| {
            dispatch
                .params
                .iter()
                .filter_map(|(param, value)| match value {
                    ParamValue::Str(value) => stray(dispatch.to.as_str(), param.as_str(), value),
                    _ => None,
                })
        });
        in_records.chain(on_their_way).next()
    }
}

impl Classes {
    /// Link what a policy compares (`resource.a == resource.b`, say), for
    /// each type in `scope`: `compared` is each comparison's reads that can
    /// hold a made-up value. A context key that can holds some type's id.
    pub(in crate::composite::joint) fn read_policies(
        &mut self,
        compared: &[Vec<Read>],
        scope: &BTreeSet<String>,
    ) {
        for reads in compared {
            for entity in scope {
                let terms: Vec<Term> = reads
                    .iter()
                    .flat_map(|read| match read {
                        Read::Resource(field) => vec![(entity.clone(), Slot::named(field))],
                        Read::Context(_) => scope.iter().map(|t| (t.clone(), Slot::Id)).collect(),
                    })
                    .collect();
                for pair in terms.windows(2) {
                    self.compared(&pair[0], &pair[1]);
                }
            }
        }
    }

    /// Tell each of `models` the class of its string fields and parameters.
    ///
    /// Nothing can tell whether values of two classes are equal: everything
    /// that compares or copies one value into another links their terms, so
    /// a walk that keeps the values of each class apart and numbers them from
    /// one in each class merges only states nothing can tell apart.
    ///
    /// A class whose values are only ever read against a record's id goes
    /// further, and its class number is odd ([`crate::model::names_no_record`]). Reading a
    /// value that names no record finds no record, whichever value it is, and
    /// no such value can become an id, so they are all one value. The record
    /// ids stay numbered.
    pub(in crate::composite::joint) fn number(&self, models: &mut BTreeMap<String, TemperModel>) {
        let mut numbers: BTreeMap<Term, usize> = BTreeMap::new();
        let reads_blank: BTreeSet<Term> = self
            .reads_blank
            .iter()
            .map(|term| self.root(term))
            .collect();
        let apart: BTreeSet<Term> = self.apart.iter().map(|term| self.root(term)).collect();
        for (entity, model) in models.iter_mut() {
            let params = model.transitions.iter().flat_map(|t| &t.string_params);
            let names: BTreeSet<String> = model
                .tracked_strings
                .iter()
                .cloned()
                .chain(params.map(|param| param.name.to_string()))
                .chain([ID.to_string()])
                .collect();
            for name in names {
                let root = self.root(&(entity.clone(), Slot::named(&name)));
                let next = (numbers.len() + 1) * 2 + usize::from(!apart.contains(&root));
                let blank = reads_blank.contains(&root);
                let class = *numbers.entry(root).or_insert(next);
                if !blank {
                    model.never_blank.insert(class);
                }
                model.string_classes.insert(name, class);
            }
        }
    }
}
