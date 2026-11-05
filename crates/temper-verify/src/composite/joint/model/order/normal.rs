//! Keeping a state's sequence numbers as how far behind the newest they are.

use std::collections::BTreeSet;
use std::sync::Arc;

use super::super::super::{Dispatch, JointState};
use super::{BEHIND, Family, Numbered, Order};
use crate::model::{Fields, ParamValue, TemperModelState};

impl Order {
    /// Whether `target` refuses `dispatch` now and whenever it lands: it
    /// carries a count not greater than a field that never goes down, and
    /// the model knows that (not both [`BEHIND`] or further behind).
    fn doomed(&self, dispatch: &Dispatch, records: &Fields<Arc<TemperModelState>>) -> bool {
        let Some(pairs) = self.refusals.get(&(dispatch.to, dispatch.action)) else {
            return false;
        };
        let target = &records[&dispatch.to];
        pairs.iter().any(|(param, field)| {
            match (target.counters.get(field), dispatch.params.get(param)) {
                (Some(held), Some(ParamValue::Count(sent))) => {
                    *sent <= *held && (*sent, *held) != (0, 0)
                }
                _ => false,
            }
        })
    }

    /// Keep each family's values in `state` as how far behind its newest
    /// they are: the newest is [`BEHIND`], and 0 is that far behind or more.
    /// A dispatch its record will refuse whenever it lands (it carries a
    /// count not greater than a field that never goes down) is first dropped:
    /// a refused dispatch changes nothing, so it is the same as delivered.
    /// Says whether it rewrote a dispatch still in flight.
    pub(in crate::composite::joint::model) fn normalize(&self, state: &mut JointState) -> bool {
        let mut rewrote = false;
        let records = &state.records;
        if !self.refusals.is_empty() {
            state
                .inflight
                .retain(|dispatch| !self.doomed(dispatch, records));
        }
        for family in &self.numbered {
            let newest = newest(family, state);
            let keep = |value: usize| BEHIND - (newest - value).min(BEHIND);
            for (entity, field) in &family.fields {
                let Some(value) = state
                    .records
                    .get(entity)
                    .and_then(|record| record.counters.get(field).copied())
                else {
                    continue;
                };
                if keep(value) != value
                    && let Some(record) = state.records.get_mut(entity)
                {
                    Arc::make_mut(record).counters.insert(*field, keep(value));
                }
            }
            for dispatch in &mut state.inflight {
                for (entity, action, param) in &family.params {
                    if &dispatch.to == entity
                        && &dispatch.action == action
                        && let Some(&ParamValue::Count(value)) = dispatch.params.get(param)
                        && keep(value) != value
                    {
                        Arc::make_mut(dispatch)
                            .params
                            .insert(*param, ParamValue::Count(keep(value)));
                        rewrote = true;
                    }
                }
            }
        }
        rewrote
    }
}

/// The largest value `family` holds in `state`, or 0 when it holds none.
fn newest(family: &Numbered, state: &JointState) -> usize {
    let mut newest = 0;
    for (entity, field) in &family.fields {
        if let Some(value) = state
            .records
            .get(entity)
            .and_then(|record| record.counters.get(field))
        {
            newest = newest.max(*value);
        }
    }
    for dispatch in &state.inflight {
        for (entity, action, param) in &family.params {
            if &dispatch.to == entity
                && &dispatch.action == action
                && let Some(ParamValue::Count(value)) = dispatch.params.get(param)
            {
                newest = newest.max(*value);
            }
        }
    }
    newest
}

/// The values `family` holds in `state`, in records and in flight.
pub(in crate::composite::joint::model) fn values(
    family: &Family,
    state: &JointState,
) -> BTreeSet<usize> {
    let mut values = BTreeSet::new();
    for (entity, field) in &family.fields {
        if let Some(value) = state
            .records
            .get(entity.as_str())
            .and_then(|r| r.counters.get(field))
        {
            values.insert(*value);
        }
    }
    for dispatch in &state.inflight {
        for (entity, action, param) in &family.params {
            if &dispatch.to == entity
                && &dispatch.action == action
                && let Some(ParamValue::Count(value)) = dispatch.params.get(param)
            {
                values.insert(*value);
            }
        }
    }
    values
}
