//! A joint state: the records in scope, and the dispatches and module calls
//! on their way.

use std::fmt;
use std::sync::Arc;

use super::env::ID;
use super::room::{Heap, block};
use crate::model::{
    Fields, ModelString, ParamValue, Renames, Sym, TemperModelAction, TemperModelState,
    names_no_record,
};

#[path = "state_rename.rs"]
mod rename;
use rename::{rename_on_their_way, rename_records};

/// The records in scope, one per type, and the dispatches and module calls
/// on their way. A move changes one record or two and the rest stay as they
/// were, so states share their records, dispatches and calls; a walk's states
/// differ only in which ones they hold.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct JointState {
    /// Each type's record.
    pub records: Fields<Arc<TemperModelState>>,
    /// Dispatches sent and not yet taken, sorted; the same dispatch may be
    /// in flight twice.
    pub inflight: Vec<Arc<Dispatch>>,
    /// Module calls started and not yet answered, sorted. Tracked only when
    /// the model makes the kernel's moves itself.
    pub calls: Vec<Arc<Call>>,
    /// Who armed each record's state timer, by sender number, for the
    /// records whose timer is still armed. Presence tracks arming even when
    /// sender identity does not affect any reaction.
    pub armed: Fields<u32>,
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
    pub to: Sym,
    /// The module.
    pub module: Sym,
    /// The trigger's declared answers; empty when it names none.
    pub on_success: Arc<[Sym]>,
    /// The action sent when the call fails, if any.
    pub on_failure: Option<Sym>,
    /// Who its answer runs as, by sender number: whoever sent the move
    /// that called the module.
    pub by: u32,
    /// The tracked fields its answers are tied to, as the record held them
    /// when the call started: the module answers about that record.
    pub asked: Fields<ModelString>,
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
    pub to: Sym,
    /// The action.
    pub action: Sym,
    /// The transition's target status, when the sender chose one.
    pub target_state: Option<Sym>,
    /// The parameters sent; a reaction's are its `args` as the source
    /// committed them.
    pub params: Fields<ParamValue>,
    /// Whether the dispatch path has read the related records it needs.
    pub read: bool,
    /// The steps this reaction chain has admitted, including this dispatch.
    /// Empty for a request that starts a new chain.
    pub chain: Arc<[ReactionStep]>,
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
        let committed = self.unread_guard.iter().flat_map(Committed::values);
        self.params.values().chain(committed)
    }
}

/// What a trigger's guard reads of its source, as the source committed:
/// the fields it names, and its status if it names that.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Committed {
    /// The source's status after its move, when the guard reads it. Holding
    /// one it doesn't read would split a state by a value nothing can see.
    pub status: Option<Sym>,
    /// The fields the guard reads, by name.
    pub fields: Fields<ParamValue>,
    /// The values of the source's move the guard reads by name, as the
    /// runtime copies them into the source's fields.
    pub params: Fields<ParamValue>,
}

impl Committed {
    /// The values it holds: its fields and the move's values.
    pub(super) fn values(&self) -> impl Iterator<Item = &ParamValue> {
        self.fields.values().chain(self.params.values())
    }

    /// The values it holds, to rename.
    pub(super) fn values_mut(&mut self) -> impl Iterator<Item = &mut ParamValue> {
        self.fields.values_mut().chain(self.params.values_mut())
    }

    /// The source record as the guard reads it.
    pub(super) fn record(&self) -> TemperModelState {
        let mut record = TemperModelState {
            status: self.status.unwrap_or_else(|| Sym::new("")),
            counters: Fields::new(),
            booleans: Fields::new(),
            lists: Fields::new(),
            strings: Fields::new(),
        };
        for (field, value) in &self.fields {
            match value {
                ParamValue::Count(count) => {
                    record.counters.insert(*field, *count);
                }
                ParamValue::Bool(value) => {
                    record.booleans.insert(*field, *value);
                }
                ParamValue::Str(text) => {
                    record.strings.insert(*field, *text);
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
            name: self.action,
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
/// the dispatches in flight; returns each value's new name. What a renaming
/// leaves as it was stays shared with the states that hold it.
pub(super) fn canonical(state: &mut JointState) -> Renames {
    let mut renamed = names_of_records(&state.records);
    renamed.name_all(strings_on_their_way(state));
    if renamed.any_moved() {
        rename_records(&mut state.records, &renamed);
        rename_on_their_way(state, &renamed);
    }
    state.inflight.sort();
    state.calls.sort();
    renamed
}

/// The new names of a state's records' strings: their ids first, since what
/// a value names decides what it is called.
fn names_of_records(records: &Fields<Arc<TemperModelState>>) -> Renames {
    let mut renamed = Renames::default();
    renamed.name_ids(record_ids(records));
    renamed.name_all(record_strings(records));
    renamed
}

/// The ids of a state's records.
fn record_ids(records: &Fields<Arc<TemperModelState>>) -> impl Iterator<Item = &ModelString> {
    records.values().filter_map(|record| record.strings.get(ID))
}

/// The strings of a state's records, in the order they are numbered.
fn record_strings(records: &Fields<Arc<TemperModelState>>) -> impl Iterator<Item = &ModelString> {
    records
        .values()
        .flat_map(|s| s.strings.values().chain(s.lists.values().flatten()))
}

/// The strings of what is on its way, in the order they are numbered.
fn strings_on_their_way(state: &JointState) -> impl Iterator<Item = &ModelString> {
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
        .chain(state.calls.iter().flat_map(|c| c.asked.values()))
}

/// Whether `holds` holds of each string [`strings_on_their_way`] gives, asked
/// in that order, with no iterator to build for the many states that hold few.
fn all_on_their_way(state: &JointState, mut holds: impl FnMut(&ModelString) -> bool) -> bool {
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
    state
        .calls
        .iter()
        .all(|call| call.asked.values().all(&mut holds))
}

/// What renumbering comes to for a list of records, which is the same whatever
/// else a state holds: records are numbered first.
#[derive(Debug)]
pub(super) struct RecordNames {
    /// The records with their new numbers.
    pub(super) records: Fields<Arc<TemperModelState>>,
    renamed: Renames,
    moved: bool,
    so_far: InOrder,
}

impl RecordNames {
    /// What renumbering `records` comes to.
    pub(super) fn of(records: &Fields<Arc<TemperModelState>>) -> Self {
        let renamed = names_of_records(records);
        let moved = renamed.any_moved();
        let mut so_far = InOrder::default();
        let mut renumbered = records.clone();
        if moved {
            rename_records(&mut renumbered, &renamed);
        }
        for value in record_ids(&renumbered) {
            so_far.fits_id(value);
        }
        for value in record_strings(&renumbered) {
            so_far.fits(value);
        }
        Self {
            records: renumbered,
            renamed,
            moved,
            so_far,
        }
    }

    /// The bytes it takes, past its records' own.
    pub(super) fn heap(&self) -> usize {
        block(size_of::<Self>())
            + block(self.records.len() * size_of::<(Sym, Arc<TemperModelState>)>())
            + self.renamed.heap()
    }

    /// Renumber `state`, which holds the records this was worked out from.
    /// `names` is somewhere to work. What is in flight is sorted already,
    /// unless `rewrote` says a dispatch was rewritten.
    pub(super) fn renumber(&self, state: &mut JointState, names: &mut Renames, rewrote: bool) {
        if !self.moved && self.so_far.holds_rest_of(state) {
            if rewrote {
                state.inflight.sort();
            }
            return;
        }
        if self.moved {
            state.records.clone_from(&self.records);
            if all_on_their_way(state, |value| matches!(value, ModelString::Literal(_))) {
                if rewrote {
                    state.inflight.sort();
                }
                return;
            }
        }
        names.clone_from(&self.renamed);
        names.name_all(strings_on_their_way(state));
        if names.any_moved() {
            rename_on_their_way(state, names);
        }
        state.inflight.sort();
        state.calls.sort();
    }
}

/// [`canonical`] when nothing needs to know what changed its name, which is
/// nothing for most states: their names are already in order. What is in
/// flight is sorted already, unless `rewrote` says a dispatch was rewritten.
pub(super) fn canonical_quietly(state: &mut JointState, rewrote: bool) {
    let mut so_far = InOrder::default();
    if !record_ids(&state.records).all(|value| so_far.fits_id(value))
        || !record_strings(&state.records).all(|value| so_far.fits(value))
        || !so_far.holds_rest_of(state)
    {
        canonical(state);
    } else if rewrote {
        state.inflight.sort();
    }
}

/// How far a state's symbols are numbered as [`canonical`] would number them:
/// in each class one, two, three, as each is first used, and each number with
/// one meaning. False too for more symbols than this keeps track of.
#[derive(Debug, Clone, Copy, Default)]
struct InOrder {
    classes: [ClassOrder; 8],
}

/// How far [`InOrder`] has got in one class. No count is a slot still free.
#[derive(Debug, Clone, Copy, Default)]
struct ClassOrder {
    class: usize,
    count: usize,
    blanks: u64,
}

impl InOrder {
    /// Whether the record id `value` carries the numbering on.
    fn fits_id(&mut self, value: &ModelString) -> bool {
        self.carries_on(value, true)
    }

    /// Whether `value`, past the record ids, carries the numbering on.
    fn fits(&mut self, value: &ModelString) -> bool {
        self.carries_on(value, false)
    }

    fn carries_on(&mut self, value: &ModelString, id: bool) -> bool {
        let (Some(class), Some(number)) = (value.class(), value.number()) else {
            return true;
        };
        let one_value = names_no_record(class);
        if number == 0 {
            return one_value && !id;
        }
        if number >= u64::BITS as usize {
            return false;
        }
        let blank = value.is_blank();
        for slot in &mut self.classes {
            if slot.count == 0 {
                *slot = ClassOrder {
                    class,
                    ..ClassOrder::default()
                };
            }
            if slot.class != class {
                continue;
            }
            return if number == slot.count + 1 {
                if one_value && !id {
                    return false;
                }
                slot.count = number;
                slot.blanks |= u64::from(blank) << number;
                true
            } else {
                number <= slot.count && (slot.blanks >> number & 1 == 1) == blank
            };
        }
        false
    }

    /// Whether what `state` has on its way carries the numbering on.
    fn holds_rest_of(mut self, state: &JointState) -> bool {
        all_on_their_way(state, |value| self.fits(value))
    }
}

/// The strings among `values`.
fn strings<'a>(
    values: impl Iterator<Item = &'a ParamValue>,
) -> impl Iterator<Item = &'a ModelString> {
    values.filter_map(|value| match value {
        ParamValue::Str(text) => Some(text),
        _ => None,
    })
}

/// One admitted reaction step. The model holds one record per type, but
/// distinguishes an entity the chain found from one it made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ReactionStep {
    /// The entity type the action runs on.
    pub entity_type: Sym,
    /// The action.
    pub action: Sym,
    /// The type for which this id was generated; `same_id` keeps its origin
    /// even through other types. `None` means the chain read the id.
    pub generated_for: Option<Sym>,
}

impl ReactionStep {
    pub(super) fn repeats(&self, taken: &Self) -> bool {
        let made = |step: &Self| step.generated_for.as_ref() == Some(&step.entity_type);
        self.entity_type == taken.entity_type
            && self.action == taken.action
            && made(self) == made(taken)
    }
}
