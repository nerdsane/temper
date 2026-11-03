//! A joint state: the records in scope, and the dispatches and module calls
//! on their way.

use std::collections::BTreeMap;
use std::fmt;

use crate::model::semantics::canonical_values;
use crate::model::{ModelString, ParamValue, TemperModelAction, TemperModelState};

/// The records in scope, one per type, and the dispatches and module calls
/// on their way.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct JointState {
    /// Each type's record.
    pub records: BTreeMap<String, TemperModelState>,
    /// Dispatches sent and not yet taken, sorted; the same dispatch may be
    /// in flight twice.
    pub inflight: Vec<Dispatch>,
    /// Module calls started and not yet answered, sorted. Tracked only when
    /// the model makes the kernel's moves itself.
    pub calls: Vec<Call>,
    /// Who armed each record's state timer, by sender number, for the
    /// records whose timer is still armed. Presence tracks arming even when
    /// sender identity does not affect any reaction.
    pub armed: BTreeMap<String, u32>,
}

impl JointState {
    /// Nothing is on its way: no dispatch and no module call.
    pub fn settled(&self) -> bool {
        self.inflight.is_empty() && self.calls.is_empty()
    }
}

/// A module call a record's trigger started, which the module answers later.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Call {
    /// The entity type whose record's trigger started it; the answer goes
    /// back to that record.
    pub to: String,
    /// The module.
    pub module: String,
    /// The trigger's declared answers; empty when it names none.
    pub on_success: Vec<String>,
    /// The action sent when the call fails, if any.
    pub on_failure: Option<String>,
    /// Who its answer runs as, by sender number: whoever sent the move
    /// that called the module.
    pub by: u32,
    /// The tracked fields its answers are tied to, as the record held them
    /// when the call started: the module answers about that record.
    pub asked: BTreeMap<String, ModelString>,
}

impl fmt::Display for Call {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {} call", self.to, self.module)?;
        for (field, value) in &self.asked {
            write!(f, " for {field}='{value}'")?;
        }
        Ok(())
    }
}

/// A dispatch on its way to a record's actor.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Dispatch {
    /// The target entity type.
    pub to: String,
    /// The action.
    pub action: String,
    /// The transition's target status, when the sender chose one.
    pub target_state: Option<String>,
    /// The parameters sent; a reaction's are its `args` as the source
    /// committed them.
    pub params: BTreeMap<String, ParamValue>,
    /// Whether the dispatch path has read the related records it needs.
    pub read: bool,
    /// The steps this reaction chain has admitted, including this dispatch.
    /// Empty for a request that starts a new chain.
    pub chain: Vec<ReactionStep>,
    /// The reaction that sent it, by its place among the model's reactions;
    /// `None` for a request from outside.
    pub via: Option<usize>,
    /// Who sends it, by sender number; Cedar decides it as them.
    pub by: u32,
    /// For a reaction whose trigger's guard reads a related record in
    /// scope: its source as it committed, until the guard is read. The
    /// runtime reads the related records after the source commits, one
    /// trigger at a time, so other moves can land first.
    pub unread_guard: Option<Committed>,
}

impl Dispatch {
    /// The values it carries: its parameters and its unread source's fields.
    pub(super) fn values(&self) -> impl Iterator<Item = &ParamValue> {
        let committed = self.unread_guard.iter().flat_map(|c| c.fields.values());
        self.params.values().chain(committed)
    }
}

/// What a trigger's guard reads of its source, as the source committed:
/// its status and the fields the guard names.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Committed {
    /// The source's status after its move.
    pub status: String,
    /// The fields the guard reads, by name.
    pub fields: BTreeMap<String, ParamValue>,
}

impl Committed {
    /// The source record as the guard reads it.
    pub(super) fn record(&self) -> TemperModelState {
        let mut record = TemperModelState {
            status: self.status.clone(),
            counters: BTreeMap::new(),
            booleans: BTreeMap::new(),
            lists: BTreeMap::new(),
            strings: BTreeMap::new(),
        };
        for (field, value) in &self.fields {
            match value {
                ParamValue::Count(count) => {
                    record.counters.insert(field.clone(), *count);
                }
                ParamValue::Bool(value) => {
                    record.booleans.insert(field.clone(), *value);
                }
                ParamValue::Str(text) => {
                    record.strings.insert(field.clone(), text.clone());
                }
                ParamValue::Fresh => {}
            }
        }
        record
    }
}

impl fmt::Display for Dispatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let action = TemperModelAction {
            name: self.action.clone(),
            target_state: None,
            params: self.params.clone(),
        };
        write!(f, "{}.{action}", self.to)?;
        if self.unread_guard.is_some() {
            f.write_str(" (trigger guard unread)")?;
        } else if !self.read {
            f.write_str(" (unread)")?;
        }
        Ok(())
    }
}

/// A joint state in words: each record, then what is on its way.
pub(super) fn render(state: &JointState) -> String {
    let mut text = state
        .records
        .iter()
        .map(|(entity, s)| format!("{entity}={s}"))
        .collect::<Vec<_>>()
        .join(" | ");
    if !state.inflight.is_empty() {
        let inflight: Vec<String> = state.inflight.iter().map(ToString::to_string).collect();
        text.push_str(&format!(" | in flight: {}", inflight.join(", ")));
    }
    if !state.calls.is_empty() {
        let calls: Vec<String> = state.calls.iter().map(ToString::to_string).collect();
        text.push_str(&format!(" | calls: {}", calls.join(", ")));
    }
    text
}

/// Renumber new strings over the whole joint state, records first, then
/// the dispatches in flight; returns each value's new name.
pub(super) fn canonical(state: &mut JointState) -> BTreeMap<ModelString, ModelString> {
    let mut dispatches = std::mem::take(&mut state.inflight);
    let mut strings: Vec<BTreeMap<String, ModelString>> = dispatches
        .iter()
        .map(|d| {
            d.params
                .iter()
                .filter_map(|(name, value)| match value {
                    ParamValue::Str(text) => Some((name.clone(), text.clone())),
                    _ => None,
                })
                .collect()
        })
        .collect();
    let renamed = {
        let committed = dispatches
            .iter_mut()
            .filter_map(|d| d.unread_guard.as_mut())
            .flat_map(|c| c.fields.values_mut())
            .filter_map(|value| match value {
                ParamValue::Str(text) => Some(text),
                _ => None,
            });
        let values = state
            .records
            .values_mut()
            .flat_map(|s| s.strings.values_mut().chain(s.lists.values_mut().flatten()))
            .chain(strings.iter_mut().flat_map(|s| s.values_mut()))
            .chain(committed)
            .chain(state.calls.iter_mut().flat_map(|c| c.asked.values_mut()));
        canonical_values(values)
    };
    for (dispatch, renamed_strings) in dispatches.iter_mut().zip(strings) {
        for (name, text) in renamed_strings {
            dispatch.params.insert(name, ParamValue::Str(text));
        }
    }
    dispatches.sort();
    state.inflight = dispatches;
    state.calls.sort();
    renamed
}

/// One admitted reaction step. The model holds one record per type, but
/// distinguishes an entity the chain found from one it made.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ReactionStep {
    /// The entity type the action runs on.
    pub entity_type: String,
    /// The action.
    pub action: String,
    /// The type for which this id was generated; `same_id` keeps its origin
    /// even through other types. `None` means the chain read the id.
    pub generated_for: Option<String>,
}

impl ReactionStep {
    pub(super) fn repeats(&self, taken: &Self) -> bool {
        let made = |step: &Self| step.generated_for.as_ref() == Some(&step.entity_type);
        self.entity_type == taken.entity_type
            && self.action == taken.action
            && made(self) == made(taken)
    }
}
