//! Renaming made-up values to their canonical names, in records and in
//! what is on its way.

use std::sync::Arc;

use super::{JointState, strings};
use crate::model::{Fields, ModelString, ParamValue, Renames, TemperModelState};

fn rewrite(renamed: &Renames, value: &mut ModelString) {
    if let Some(name) = renamed.get(value) {
        *value = *name;
    }
}

fn moved(renamed: &Renames, value: &ModelString) -> bool {
    renamed.get(value).is_some_and(|name| name != value)
}

pub(super) fn rename_records(records: &mut Fields<Arc<TemperModelState>>, renamed: &Renames) {
    for record in records.values_mut() {
        let touched = record
            .strings
            .values()
            .chain(record.lists.values().flatten())
            .any(|value| moved(renamed, value));
        if touched {
            let record = Arc::make_mut(record);
            record
                .strings
                .values_mut()
                .for_each(|value| rewrite(renamed, value));
            record
                .lists
                .values_mut()
                .flatten()
                .for_each(|value| rewrite(renamed, value));
        }
    }
}

pub(super) fn rename_on_their_way(state: &mut JointState, renamed: &Renames) {
    let rewrite_param = |value: &mut ParamValue| {
        if let ParamValue::Str(text) = value {
            rewrite(renamed, text);
        }
    };
    for dispatch in &mut state.inflight {
        if strings(dispatch.values()).any(|value| moved(renamed, value)) {
            let dispatch = Arc::make_mut(dispatch);
            dispatch.params.values_mut().for_each(rewrite_param);
            if let Some(committed) = &mut dispatch.unread_guard {
                committed.values_mut().for_each(rewrite_param);
            }
            if let Some(caller) = &mut dispatch.caller {
                caller.values_mut().for_each(rewrite_param);
            }
        }
    }
    for call in &mut state.calls {
        let caller = call
            .caller
            .iter()
            .flat_map(|caller| strings(caller.values()));
        if call
            .asked
            .values()
            .chain(caller)
            .any(|value| moved(renamed, value))
        {
            let call = Arc::make_mut(call);
            call.asked
                .values_mut()
                .for_each(|value| rewrite(renamed, value));
            if let Some(caller) = &mut call.caller {
                caller.values_mut().for_each(rewrite_param);
            }
        }
    }
}

/// The strings of what is on its way, in the order they are numbered.
pub(super) fn strings_on_their_way(state: &JointState) -> impl Iterator<Item = &ModelString> {
    state
        .inflight
        .iter()
        .flat_map(|d| strings(d.params.values()))
        .chain(
            state
                .inflight
                .iter()
                .flat_map(|d| d.unread_guard.iter().flat_map(|c| strings(c.values()))),
        )
        .chain(
            state
                .inflight
                .iter()
                .flat_map(|d| d.caller.iter().flat_map(|c| strings(c.values()))),
        )
        .chain(state.calls.iter().flat_map(|c| {
            let caller = c.caller.iter().flat_map(|caller| strings(caller.values()));
            c.asked.values().chain(caller)
        }))
}

/// Whether `holds` holds of each string [`strings_on_their_way`] gives, asked
/// in that order, with no iterator to build for the many states that hold few.
pub(super) fn all_on_their_way(
    state: &JointState,
    mut holds: impl FnMut(&ModelString) -> bool,
) -> bool {
    let mut param = |value: &ParamValue| match value {
        ParamValue::Str(text) => holds(text),
        _ => true,
    };
    for dispatch in &state.inflight {
        if !dispatch.params.values().all(&mut param) {
            return false;
        }
    }
    for dispatch in &state.inflight {
        if let Some(committed) = &dispatch.unread_guard
            && !committed.values().all(&mut param)
        {
            return false;
        }
    }
    for dispatch in &state.inflight {
        if let Some(caller) = &dispatch.caller
            && !caller.values().all(&mut param)
        {
            return false;
        }
    }
    for call in &state.calls {
        if !call
            .asked
            .values()
            .all(|value| param(&ParamValue::Str(*value)))
        {
            return false;
        }
        if let Some(caller) = &call.caller
            && !caller.values().all(&mut param)
        {
            return false;
        }
    }
    true
}
