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
        }
    }
    for call in &mut state.calls {
        if call.asked.values().any(|value| moved(renamed, value)) {
            Arc::make_mut(call)
                .asked
                .values_mut()
                .for_each(|value| rewrite(renamed, value));
        }
    }
}
