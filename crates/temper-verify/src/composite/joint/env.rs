//! One entity of a joint state as a predicate environment, and how a
//! reference finds its record.

use std::collections::{BTreeMap, BTreeSet};

use temper_spec::predicate::{Env, Val, VarKind};

use super::JointState;
use crate::model::{TemperModel, TemperModelState};

/// The key under which each record keeps its own id among its strings.
pub(super) const ID: &str = "Id";

/// What a reference's id field holds when there is one record of a type: the
/// id of that record, which every reference finds.
const ONE_ID: &str = "#1";

/// What an evaluation may not see. The runtime reads related records on the
/// dispatch path and the record's own state in its actor, at different
/// moments, so each step sees only its own half.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Blind {
    /// Everything is read from the joint state.
    Nothing,
    /// The record's own status and variables are unknown (the dispatch path).
    Own,
    /// Related records are unknown (the actor, which uses what the dispatch
    /// path read earlier).
    Related,
}

/// Which record a reference finds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Records {
    /// The one whose id its id field holds. Each record has an id, and walks
    /// start from every way the ids can be equal or differ.
    ById,
    /// The one record of its type, always. The drop check walks a whole
    /// trigger-graph component, where the ways a dozen ids can be equal or
    /// differ are too many to start from.
    One,
}

/// Whether a reference finds its record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Found {
    /// The id field holds the related record's id.
    Yes,
    /// It holds another id, or none: no such record.
    Missing,
    /// The model does not track the id field.
    Unknown,
}

/// The record of type `related` that `owner`'s `id_field` names: found when
/// the field holds that record's id (`Id` reads `owner`'s own id, which is
/// how `same_id` resolves), or always when there is one record of a type.
pub(super) fn resolve(
    models: &BTreeMap<String, TemperModel>,
    records: Records,
    state: &JointState,
    owner: &str,
    id_field: &str,
    related: &str,
) -> Found {
    resolve_from(
        models,
        records,
        state,
        state.records.get(owner),
        owner,
        id_field,
        related,
    )
}

/// [`resolve`], with `owner`'s record as `source` holds it.
fn resolve_from(
    models: &BTreeMap<String, TemperModel>,
    records: Records,
    state: &JointState,
    source: Option<&TemperModelState>,
    owner: &str,
    id_field: &str,
    related: &str,
) -> Found {
    let (Some(source), Some(target)) = (source, state.records.get(related)) else {
        return Found::Unknown;
    };
    if records == Records::One {
        return Found::Yes;
    }
    let id = if matches!(id_field, "Id" | "id") {
        source.strings.get(ID)
    } else if models[owner].tracked_strings.contains(id_field) {
        source.strings.get(id_field)
    } else {
        return Found::Unknown;
    };
    match (id, target.strings.get(ID)) {
        (Some(id), Some(target_id)) if id == target_id => Found::Yes,
        (Some(_), Some(_)) => Found::Missing,
        _ => Found::Unknown,
    }
}

/// One entity of a joint state as a predicate environment: its own
/// variables as the per-entity model reads them, and the related entities'
/// statuses and tracked variables from the joint state, when the reference
/// finds them.
pub(super) struct JointEnv<'a> {
    pub(super) models: &'a BTreeMap<String, TemperModel>,
    pub(super) records: Records,
    /// With one record of a type, each entity's id fields that references
    /// use; empty otherwise.
    pub(super) refs: &'a BTreeMap<String, BTreeSet<String>>,
    pub(super) state: &'a JointState,
    pub(super) owner: &'a str,
    pub(super) blind: Blind,
    /// The owner's record as this evaluation reads it, when not as the
    /// joint state holds it now: a trigger's source as it committed.
    pub(super) own: Option<&'a TemperModelState>,
    /// A reference whose id field names another record than the one the
    /// model holds of its type reads as unknown, not as no record.
    pub(super) unheld_unknown: bool,
}

fn var_of<'a>(model: &TemperModel, state: &'a TemperModelState, name: &str) -> Val<'a> {
    if matches!(name, "Id" | "id") {
        return state.strings.get(ID).map_or(Val::Unknown, |s| s.val());
    }
    match model.var_kinds.get(name) {
        Some(VarKind::Counter) => Val::Num(state.counters.get(name).copied().unwrap_or(0) as f64),
        Some(VarKind::Bool) => Val::Bool(state.booleans.get(name).copied().unwrap_or(false)),
        Some(VarKind::List) => state.lists.get(name).map_or_else(
            || Val::Values(Vec::new()),
            |items| Val::Values(items.iter().map(|s| s.val()).collect()),
        ),
        Some(VarKind::Str) => state
            .strings
            .get(name)
            .map_or(Val::Unknown, |value| value.val()),
        Some(VarKind::Num) | None => Val::Unknown,
    }
}

impl JointEnv<'_> {
    /// The owner's record as this evaluation reads it.
    fn own_record(&self) -> Option<&TemperModelState> {
        self.own.or_else(|| self.state.records.get(self.owner))
    }

    /// The related record `id_field` names, if the reference finds one;
    /// `None` when it cannot be known here.
    fn related(&self, entity_type: &str, id_field: &str) -> Option<Option<&TemperModelState>> {
        if self.blind == Blind::Related {
            return None;
        }
        match resolve_from(
            self.models,
            self.records,
            self.state,
            self.own_record(),
            self.owner,
            id_field,
            entity_type,
        ) {
            Found::Yes => Some(self.state.records.get(entity_type)),
            Found::Missing if self.unheld_unknown => None,
            Found::Missing => Some(None),
            Found::Unknown => None,
        }
    }
}

impl Env for JointEnv<'_> {
    fn status(&self) -> Val<'_> {
        if self.blind == Blind::Own {
            return Val::Unknown;
        }
        match self.own_record() {
            Some(record) => Val::Str(&record.status),
            None => Val::Unknown,
        }
    }

    fn var(&self, name: &str) -> Val<'_> {
        if self.blind == Blind::Own {
            return Val::Unknown;
        }
        if self
            .refs
            .get(self.owner)
            .is_some_and(|fields| fields.contains(name))
        {
            return Val::Str(ONE_ID);
        }
        match self.own_record() {
            Some(record) => var_of(&self.models[self.owner], record, name),
            None => Val::Unknown,
        }
    }

    fn cross_statuses(&self, entity_type: &str, id_field: &str) -> Option<Vec<Val<'_>>> {
        let record = self.related(entity_type, id_field)?;
        Some(vec![record.map_or(Val::Null, |s| Val::Str(&s.status))])
    }

    fn cross_fields(&self, entity_type: &str, id_field: &str, field: &str) -> Option<Vec<Val<'_>>> {
        let record = self.related(entity_type, id_field)?;
        let model = self.models.get(entity_type)?;
        Some(vec![record.map_or(Val::Null, |s| var_of(model, s, field))])
    }
}
