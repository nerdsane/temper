//! A timer acts with the standing its setter has when it fires
//! (`timer_standing.rs` in the server). Cedar is asked whether the setter
//! may `read` the record, as their timer asks it ([`Cause::timer_fire`]):
//! as the record is before the move that sets the timer, and again when it
//! fires. A setter who could read it then and cannot now has lost their
//! standing, and the timer's move is not made. A timer a declared service
//! sets acts for no one and fires as before.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::Ordering;

use serde_json::Value;
use temper_authz::{Asker, Attr, Cause, May, SendAnalysis};
use temper_spec::automaton::Automaton;

use super::super::env::ID;
use super::super::model::copies::base;
use super::super::room::Heap;
use super::{Question, Senders, symbol};
use crate::model::{ModelString, TemperModelState};

/// Cedar's name for reading a record.
pub(super) const READ: &str = "read";

/// What a timer's setter had at stake on its record when they set it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::composite::joint) enum Stake {
    /// They could not read it, or the timer acts for no one: it fires.
    None,
    /// They could read it: it fires only while they still can.
    Read,
    /// Cedar may have answered either way.
    Either,
}

impl Stake {
    /// What a record's timer holds when `armer`, a sender out of any chain,
    /// sets it with this at stake.
    pub(in crate::composite::joint) fn arm(self, armer: u32) -> u32 {
        let stake = match self {
            Stake::None => 0,
            Stake::Read => 1,
            Stake::Either => 2,
        };
        (armer & !1) << 1 | stake
    }

    /// Who set a timer that holds `armed`, and what they had at stake.
    pub(in crate::composite::joint) fn of(armed: u32) -> (u32, Stake) {
        let stake = match armed & 3 {
            0 => Stake::None,
            1 => Stake::Read,
            _ => Stake::Either,
        };
        ((armed >> 1) & !1, stake)
    }
}

/// The types in `scope` whose timers a setter's standing may stop: they
/// have a state timer, and whether one of their records may be read, as a
/// timer asks it, can turn on the record or on who asks. Elsewhere the
/// answer when the timer fires is the answer when it was set.
pub(in crate::composite::joint) fn standing<'a>(
    by_name: &BTreeMap<&str, &Automaton>,
    scope: impl IntoIterator<Item = &'a String>,
    policies: Option<&SendAnalysis>,
) -> BTreeSet<String> {
    let Some(policies) = policies else {
        return BTreeSet::new();
    };
    let none = BTreeMap::new();
    let cause = Cause::timer_fire();
    scope
        .into_iter()
        .filter(|entity| !by_name[entity.as_str()].state_timeouts.is_empty())
        .filter(|entity| {
            policies.may(&Asker::Outside, READ, entity, &none, Some(&cause)) == May::Maybe
        })
        .cloned()
        .collect()
}

/// The fields of `automaton` the policies read. Whether a timer's setter
/// may still read its record turns on them, so a type in [`standing`]
/// tracks them.
pub(in crate::composite::joint) fn policy_fields(
    automaton: &Automaton,
    policies: Option<&SendAnalysis>,
) -> BTreeSet<String> {
    let Some(policies) = policies else {
        return BTreeSet::new();
    };
    let fields = automaton.state.iter().map(|s| &s.name);
    fields.filter(|f| policies.reads_attr(f)).cloned().collect()
}

impl Senders {
    /// What `by`, setting a timer on `record` of `entity` before its move,
    /// has at stake: nothing when `entity`'s standing cannot change or a
    /// declared service sets it.
    pub(in crate::composite::joint) fn stake(
        &self,
        by: u32,
        entity: &str,
        record: &TemperModelState,
    ) -> Stake {
        let caller = matches!(
            self.askers.get(by as usize / 2),
            Some(Asker::Outside | Asker::OutsideAs(_))
        );
        if !caller || !self.reads_standing(entity) {
            return Stake::None;
        }
        match self.may_read(by, entity, record) {
            May::Yes => Stake::Read,
            May::No => Stake::None,
            May::Maybe => Stake::Either,
        }
    }

    /// Whether `entity`'s timers ask their setter's standing.
    fn reads_standing(&self, entity: &str) -> bool {
        self.may
            .as_ref()
            .and_then(|may| may.get(base(entity)))
            .is_some_and(|actions| actions.contains_key(READ))
    }

    /// Whether `by` may read `record` of `entity`, as a timer it set asks
    /// it: as the policies alone answer, or, when that depends on the
    /// record, as they answer given its id and tracked fields.
    pub(in crate::composite::joint) fn may_read(
        &self,
        by: u32,
        entity: &str,
        record: &TemperModelState,
    ) -> May {
        let by = by & !1;
        let may = self.may(by, entity, READ);
        let (May::Maybe, Some(policies), Some(asker)) =
            (may, &self.policies, self.askers.get(by as usize / 2))
        else {
            return may;
        };
        let entity = base(entity);
        let question = (
            by,
            entity.to_string(),
            READ.to_string(),
            String::new(),
            None,
            None,
            record.strings.clone(),
            record.booleans.clone(),
        );
        if let Some(may) = self
            .decided
            .read()
            .expect("a walk thread panicked")
            .get(&question)
        {
            return *may;
        }
        let literals = literals(record.strings.values());
        let attrs = self.record_attrs(entity, record, &literals);
        let may = policies.may(asker, READ, entity, &attrs, Some(&Cause::timer_fire()));
        self.remember(question, may)
    }

    /// `record` of `entity` as Cedar reads it: its id and tracked strings,
    /// each a value the model made up known only as none of the `literals`
    /// its field can meet, and its tracked booleans.
    pub(super) fn record_attrs(
        &self,
        entity: &str,
        record: &TemperModelState,
        literals: &BTreeSet<String>,
    ) -> BTreeMap<String, Attr> {
        let strings = record.strings.iter().map(|(field, value)| {
            let value = self.symbol_in(value, entity, field, literals);
            let field = if field == ID { "id" } else { field.as_str() };
            (field.to_string(), value)
        });
        let booleans = record
            .booleans
            .iter()
            .map(|(field, value)| (field.clone(), Attr::Value(Value::Bool(*value))));
        strings.chain(booleans).collect()
    }

    /// `value` of `entity`'s `field` as Cedar may read it, of `literals`
    /// the ones the field can meet.
    pub(super) fn symbol_in(
        &self,
        value: &ModelString,
        entity: &str,
        field: &str,
        literals: &BTreeSet<String>,
    ) -> Attr {
        let key = (base(entity).to_string(), field.to_string());
        let meets = self.meets.get(&key);
        let known = meets.map_or_else(BTreeSet::new, |meets| {
            literals.intersection(meets).cloned().collect()
        });
        symbol(value, &known)
    }

    /// Keep `may` as the answer to `question`, counting what it holds.
    pub(super) fn remember(&self, question: Question, may: May) -> May {
        let entry = super::super::room::map_bytes::<Question, May>(1)
            + question.1.heap()
            + question.2.heap()
            + question.3.heap()
            + question.4.heap()
            + question.5.heap()
            + question.6.heap()
            + question.7.heap();
        self.decided_bytes.fetch_add(entry, Ordering::Relaxed);
        self.decided
            .write()
            .expect("a walk thread panicked")
            .insert(question, may);
        may
    }
}

/// The literals among `values`: a value the model made up is none of those
/// its field can meet, and any other it may be.
pub(super) fn literals<'a>(values: impl IntoIterator<Item = &'a ModelString>) -> BTreeSet<String> {
    values
        .into_iter()
        .filter_map(|value| match value {
            ModelString::Literal(text) if !text.is_empty() => Some(text.clone()),
            _ => None,
        })
        .collect()
}
