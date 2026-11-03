//! The dry run of one carry on the records it applies to, before the swap.

use std::collections::BTreeMap;

use serde_json::Value;
use temper_authz::AuthzEngine;
use temper_jit::table::{RelatedMap, Standing, TransitionTable};
use temper_runtime::tenant::TenantId;

use super::then_move::{cedar_lets, then_maker};
use super::{Carried, CarryMove, SpecChangePlan, SpecChangeRefused, answer};
use crate::entity_actor::{
    EntityState, broken_invariant, build_eval_context_with_xref, process_action_with_xref,
};
use crate::state::ServerState;
use crate::state::dispatch::bind_authenticated_params;

impl ServerState {
    /// Try one carry on the records it applies to, as they are stored now,
    /// with the move code every move runs and the related records it reads,
    /// as a move resolves them, then the carry's `then` move, which `cedar`
    /// must let its maker ([`then_maker`]) make. Each record it moves must
    /// hold to the new version's invariants after each move and have a way
    /// forward from where it ends.
    ///
    /// One record is tried for each group. The records of a group share
    /// their state and the values of every name the carry, the new version's
    /// moves and invariants, and its related records' references read
    /// ([`read_names`]), so the dry run ends the same way for each of them.
    /// A record is read whole from its journal, and a store read took 0.35
    /// to 0.8 s on a busy app drive (the Factory, 26 Oct 2026): one read
    /// per record would hold an install for minutes. A group whose first
    /// record cannot be read is tried with the next.
    pub(super) async fn try_carry(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        (new, cedar): (&TransitionTable, &AuthzEngine),
        carried: Carried<'_>,
        plan: &mut SpecChangePlan,
        refused: &mut SpecChangeRefused,
    ) {
        let params = serde_json::to_value(&carried.carry.set).unwrap_or_default();
        let mut refs = new.related_refs(&carried.action);
        for action in new.caller_moves() {
            for reference in new.related_refs(action) {
                if !refs.contains(&reference) {
                    refs.push(reference);
                }
            }
        }
        let mut failures: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut no_one = 0;
        for group in carried.groups {
            let mut ids = group.into_iter();
            let mut tried = None;
            for id in ids.by_ref() {
                match self.current_record_state(tenant, entity_type, &id).await {
                    Some(record) => {
                        tried = Some((id, record));
                        break;
                    }
                    None => failures
                        .entry("it could not be read".to_string())
                        .or_default()
                        .push(id),
                }
            }
            let Some((id, mut record)) = tried else {
                continue;
            };
            let group: Vec<String> = std::iter::once(id).chain(ids).collect();
            let related = self
                .related_statuses_of(tenant, new, refs.clone(), &record.fields)
                .await;
            let why = if let Err(why) =
                dry_move(&mut record, new, &carried.action, &params, &related)
            {
                Some(format!("the carry {why}"))
            } else {
                // A carry that sets a field naming a related record points
                // the record at another one, which its next moves read.
                let repointed = carried
                    .carry
                    .set
                    .keys()
                    .any(|name| refs.iter().any(|(_, field)| field == name));
                let related = if repointed {
                    self.related_statuses_of(tenant, new, refs.clone(), &record.fields)
                        .await
                } else {
                    related
                };
                let maker = carried.carry.then.as_deref().map(|then| {
                    let maker = then_maker(
                        &carried.action,
                        carried.carry.acts_for.as_deref(),
                        &record.fields,
                    );
                    (then, maker)
                });
                match maker {
                    None => way_forward(&record, &related, new),
                    // No one to act for: the record stays where the carry
                    // puts it, which must have a way forward of its own.
                    Some((_, None)) => {
                        no_one += group.len();
                        way_forward(&record, &related, new)
                    }
                    Some((then, Some(maker))) => {
                        let asked = match self
                            .build_authz_resource_attrs(
                                tenant,
                                entity_type,
                                &group[0],
                                &record.status,
                                &record.fields,
                            )
                            .await
                        {
                            Ok(attrs) => {
                                cedar_lets(cedar, tenant, &maker, then, entity_type, &attrs)
                                    .map_err(|why| format!("is refused by Cedar: {why}"))
                            }
                            Err(why) => Err(format!("could not be put to Cedar: {why}")),
                        };
                        // Its parameters are bound as the move binds them.
                        let made = asked
                            .and_then(|()| {
                                let params = Value::Object(Default::default());
                                bind_authenticated_params(new, then, params, &maker)
                                    .map_err(|error| format!("is refused: {error}"))
                            })
                            .and_then(|params| dry_move(&mut record, new, then, &params, &related));
                        match made {
                            Err(why) => Some(format!("its then move {then} {why}")),
                            Ok(()) => way_forward(&record, &related, new),
                        }
                    }
                }
            };
            match why {
                Some(why) => failures.entry(why).or_default().extend(group),
                None => plan
                    .carries
                    .extend(group.into_iter().map(|entity_id| CarryMove {
                        entity_type: entity_type.to_string(),
                        entity_id,
                        action: carried.action.clone(),
                        params: params.clone(),
                        then: carried.carry.then.clone(),
                        acts_for: carried.carry.acts_for.clone(),
                    })),
            }
        }
        if no_one > 0 {
            plan.warnings.push(format!(
                "{entity_type} in {}: the [[carry]]'s then move is not made for a record whose \
                 `{}` is empty, which stays in {} ({no_one} now)",
                carried.status,
                carried.carry.acts_for.as_deref().unwrap_or_default(),
                carried.carry.target()
            ));
        }
        for (why, ids) in failures {
            refused.reasons.push(answer::with_records(
                &format!(
                    "{entity_type} in {}: the [[carry]] from {} cannot carry these records: {why}",
                    carried.status, carried.carry.from
                ),
                &ids,
            ));
        }
    }
}

/// Make `action` on `record` with the move code every move runs, and check
/// the record it leaves against the version's invariants: why not, if not.
fn dry_move(
    record: &mut EntityState,
    new: &TransitionTable,
    action: &str,
    params: &Value,
    related: &RelatedMap,
) -> Result<(), String> {
    let moved = process_action_with_xref(record, new, action, params, related);
    if !moved.success {
        return Err(format!("is refused: {}", moved.error.unwrap_or_default()));
    }
    match broken_invariant(record, new) {
        Some(broken) => Err(format!("would break {broken}")),
        None => Ok(()),
    }
}

/// Why a carried record would have no way forward from where it ends, if it
/// would not.
fn way_forward(
    record: &EntityState,
    related: &RelatedMap,
    new: &TransitionTable,
) -> Option<String> {
    let ctx = build_eval_context_with_xref(record, related, new, None);
    match new.standing(&record.status, &ctx) {
        Standing::Free | Standing::Rests => None,
        Standing::Stuck(blocked) => Some(format!(
            "once carried to {} it would still have no way forward: {}",
            record.status,
            answer::blocked_moves(&blocked)
        )),
        Standing::Gone => Some(format!(
            "once carried it would be in {}, which is not a state of the new version",
            record.status
        )),
    }
}
