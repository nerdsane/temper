//! The digest a cascade's verdict is kept by.

use std::collections::BTreeSet;

use sha2::{Digest, Sha256};
use temper_spec::predicate::Name;

use super::{CascadeLevel, CascadeResult, VerificationCascade};
use crate::model::{self, LivenessKind, TemperModel};

/// What a kept verdict is found by, and what it is checked against before
/// it stands for this spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeptKey {
    /// The digest a verdict is kept under.
    pub digest: String,
    /// The places each silent move leaves, which the digest leaves out.
    silent: Vec<Vec<String>>,
    /// The terminal places.
    terminal: Vec<String>,
    /// The places a `NoDeadlock` liveness requires a move out of.
    watched: Vec<String>,
}

impl KeptKey {
    /// Whether `kept`, a passing verdict kept under this digest, is this
    /// spec's verdict too. The two specs differ at most in their silent
    /// moves, which change no state, so it is unless one of this spec's
    /// silent moves leaves no place the kept walk reached (the move is
    /// dead), leaves a reached terminal place (it breaks `Terminal`), or
    /// the two differ in whether a silent move leaves a reached place that
    /// a `NoDeadlock` liveness watches and no other move leaves. A verdict kept without
    /// its places stands only for a spec with no silent moves: its digest
    /// held every move.
    pub fn fits(&self, kept: &CascadeResult) -> bool {
        let reached = kept
            .levels
            .iter()
            .find(|level| level.level == CascadeLevel::ModelCheck)
            .and_then(|level| level.verification.as_ref())
            .and_then(|verification| verification.reached.as_ref());
        let Some(reached) = reached else {
            return self.silent.is_empty();
        };
        let leaves = |place: &String| self.silent.iter().any(|from| from.contains(place));
        self.silent.iter().all(|from| {
            from.iter().any(|place| reached.places.contains(place))
                && !from
                    .iter()
                    .any(|place| reached.places.contains(place) && self.terminal.contains(place))
        }) && reached
            .stuck
            .iter()
            .filter(|place| self.watched.contains(place))
            .all(|place| leaves(place) == reached.left_silently.contains(place))
    }
}

impl VerificationCascade {
    /// The key of what the levels read: the model L0, L1 and L2 walk, the
    /// one L3 walks, and how far each level goes. Every level reads only
    /// these, so two specs with one digest get one verdict, and a change
    /// none of them reads (the text of a string no rule reads, a comment,
    /// the order of sections) keeps it. The budget is left out: a check
    /// that ran out of it has no verdict to keep.
    ///
    /// The digest leaves out silent moves ([`TemperModel::is_silent`]), so
    /// a move added that stays in place and writes nothing a rule reads
    /// keeps it too; [`KeptKey::fits`] decides whether the kept verdict
    /// stands for the spec with that move.
    ///
    /// A rule that reads a related record (`Type[id].status`) names its
    /// type here by first use, not by name: the levels read the related
    /// record's status without knowing its type, so the same spec in two
    /// apps, whose types differ only by the app's prefix, has one digest.
    ///
    /// `None` when the cascade reads more than its models (an actor
    /// simulation, path extraction or a composite scope), or the spec does
    /// not parse.
    pub fn kept_key(&self) -> Option<KeptKey> {
        if self.actor_sim_runner.is_some()
            || self.path_extraction_config.is_some()
            || self.composite_scope.is_some()
        {
            return None;
        }
        let built = model::build_model_from_ioa(&self.ioa_source, self.max_counter).ok()?;
        let silent = (0..built.transitions.len())
            .filter(|&index| built.is_silent(index))
            .map(|index| built.transitions[index].from_states.clone())
            .collect();
        let model = as_read(&built);
        let prop_model = as_read(&model::build_model_from_ioa(&self.ioa_source, 2).ok()?);
        let related = related_types(&model);
        let settings = format!(
            "max_counter={} sim_seeds={} sim_ticks={} prop_test_cases={} prop_test_max_steps={} fail_fast={}",
            self.max_counter,
            self.sim_seeds,
            self.sim_ticks,
            self.prop_test_cases,
            self.prop_test_max_steps,
            self.fail_fast,
        );
        // Each part is length-prefixed so no two inputs run together into
        // the same bytes.
        let mut hasher = Sha256::new();
        for part in [
            by_first_use(&model, &related),
            by_first_use(&prop_model, &related),
            settings,
        ] {
            hasher.update((part.len() as u64).to_le_bytes());
            hasher.update(part.as_bytes());
        }
        Some(KeptKey {
            digest: format!("{:x}", hasher.finalize()),
            silent,
            terminal: built.terminal.clone(),
            watched: built
                .liveness
                .iter()
                .filter_map(|live| match &live.kind {
                    LivenessKind::NoDeadlock { from } => Some(from.clone()),
                    LivenessKind::ReachesState { .. } | LivenessKind::CanReach { .. } => None,
                })
                .flatten()
                .collect(),
        })
    }
}

/// `model` as its levels read it: without its silent moves, and with the
/// kind of only those fields a guard or rule names or the model holds. A
/// level looks a kind up only for a field an expression names, so a field
/// declared and never read adds nothing.
fn as_read(model: &TemperModel) -> TemperModel {
    let mut model = model.without_silent();
    let mut named = BTreeSet::new();
    let guards = model.transitions.iter().map(|t| &t.guard);
    let rules = model
        .invariants
        .iter()
        .map(|i| &i.assert)
        .chain(model.entry_rules.iter().map(|rule| &rule.assert));
    for expr in guards.chain(rules) {
        expr.for_each_name(&mut |name| match name {
            Name::Var(field)
            | Name::CrossStatus {
                id_field: field, ..
            }
            | Name::CrossField {
                id_field: field, ..
            } => {
                named.insert(field.to_string());
            }
            Name::Status | Name::Param(_) => {}
        });
    }
    let kept: BTreeSet<String> = model
        .var_kinds
        .keys()
        .filter(|field| {
            named.contains(*field) || model.holds(field) || model.tracked_strings.contains(*field)
        })
        .cloned()
        .collect();
    model.var_kinds.retain(|field, _| kept.contains(field));
    model
}

/// The types `model`'s guards and rules read a related record of, in
/// first-use order.
fn related_types(model: &TemperModel) -> Vec<String> {
    let guards = model.transitions.iter().map(|t| &t.guard);
    let rules = model
        .invariants
        .iter()
        .map(|i| &i.assert)
        .chain(model.entry_rules.iter().map(|rule| &rule.assert));
    let mut types: Vec<String> = Vec::new();
    for (entity_type, _) in guards.chain(rules).flat_map(|expr| expr.related_types()) {
        if !types.contains(&entity_type) {
            types.push(entity_type);
        }
    }
    types
}

/// `model` as it prints, with each of `related` named by its place in it.
/// Only a related record's operand prints an `entity_type` field.
fn by_first_use(model: &TemperModel, related: &[String]) -> String {
    let mut text = format!("{model:?}");
    for (place, entity_type) in related.iter().enumerate() {
        text = text.replace(
            &format!("entity_type: {entity_type:?}"),
            &format!("entity_type: \"#{place}\""),
        );
    }
    text
}
