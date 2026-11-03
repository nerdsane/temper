//! The check a new version of a spec passes before it replaces the installed
//! one, run by every install route before it writes or swaps anything.
//!
//! A new version may change anything, so long as no stored record is left
//! with no way forward: in a state the version drops, or in a state where
//! moves start and none is possible for the record's values though one was
//! under the installed version. The version says where such records go with
//! `[[carry]]` sections. Before anything is written, each carry is tried on
//! each record it applies to with the move code every move runs
//! ([`process_action`]): the carried record must hold to the new version's
//! invariants and have a way forward. The install then answers and the
//! version serves; the carries run after it in the background, as moves of
//! the platform journaled like any other ([`carry_pass`]).
//!
//! Records are read once per type, and only in the states a change can
//! affect: those the version drops, those a move with changed conditions
//! starts from ([`TransitionTable::move_conditions`]), and those a carry of
//! either version starts from ([`reads`]). One grouped catalog query covers
//! each record whose catalog row is current; each other record is read on
//! its own. A carry is tried on one record of each group.
//!
//! Reported, not refused: records the installed version already leaves with
//! no way forward, moves that are no longer possible for records that keep
//! another, and what is not checked: a move whose guard reads a related
//! record, which the stored record alone cannot answer, and a record that
//! cannot be read. An install that removes a whole type is not checked.

mod answer;
mod carry_pass;
mod dry_run;
mod reads;
mod records;
mod then_move;

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use temper_authz::AuthzEngine;
use temper_jit::table::{Standing, TransitionTable};
use temper_runtime::tenant::TenantId;
use temper_spec::automaton::{Automaton, Carry, parse_automaton};

use crate::state::ServerState;
use reads::{affected_states, carry_moves, read_names, reads_related};

pub use answer::{SpecChangeRefused, StuckRecords};
pub use carry_pass::CarryReport;

/// A new version of one entity type's spec, to be checked against the
/// version installed for it.
#[derive(Debug, Clone)]
pub struct SpecChange {
    /// The entity type.
    pub entity_type: String,
    /// The new version.
    pub proposed: Automaton,
}

/// One `[[carry]]` move on one record, made after the swap.
#[derive(Debug, Clone, PartialEq)]
pub struct CarryMove {
    /// The record's type.
    pub entity_type: String,
    /// The record.
    pub entity_id: String,
    /// The carry's move, `carry:<from>:<n>`.
    pub action: String,
    /// The values the carry sets.
    pub params: Value,
    /// The action the platform makes on the record right after the carry.
    pub then: Option<String>,
    /// The field naming the user the `then` move is made for.
    pub acts_for: Option<String>,
}

/// What a check found: the carries it tried, and what it reports.
#[derive(Debug, Clone, Default)]
pub struct SpecChangePlan {
    /// The carry moves, each tried already on the record as it is now.
    pub carries: Vec<CarryMove>,
    /// What the check found and did not refuse.
    pub warnings: Vec<String>,
}

/// The records of one type in one state that a carry applies to, in groups
/// that hold the same values of every name the check reads.
struct Carried<'a> {
    status: String,
    action: String,
    carry: &'a Carry,
    groups: Vec<Vec<String>>,
}

/// What a type's new version is checked with: the installed and new tables,
/// and the Cedar policies that decide its moves once it serves.
struct Checked<'a> {
    tables: (&'a TransitionTable, &'a TransitionTable),
    cedar: &'a AuthzEngine,
}

impl ServerState {
    /// The new versions among `sources`, each `(entity_type, IOA source)`:
    /// those of types `tenant` has installed whose source differs, parsed.
    pub fn spec_changes(
        &self,
        tenant: &TenantId,
        sources: &[(&str, &str)],
    ) -> Result<Vec<SpecChange>, String> {
        let registry = self.registry.read().expect("registry lock poisoned");
        let mut changes = Vec::new();
        for (entity_type, source) in sources {
            let Some(installed) = registry.get_spec(tenant, entity_type) else {
                continue;
            };
            if installed.ioa_source == *source {
                continue;
            }
            let proposed = parse_automaton(source)
                .map_err(|error| format!("spec {entity_type} failed to parse: {error}"))?;
            changes.push(SpecChange {
                entity_type: entity_type.to_string(),
                proposed,
            });
        }
        Ok(changes)
    }

    /// Check each new version against the version `tenant` has installed for
    /// its type and the records stored under it. A type with no installed
    /// version passes. Nothing is written.
    ///
    /// `policies` is the tenant's Cedar text once the install serves, when
    /// the install changes it; a carry's `then` move is asked of it, so a
    /// move the new policies refuse is refused here and not after the swap.
    /// `None` asks the policies loaded now.
    pub async fn check_spec_changes(
        &self,
        tenant: &TenantId,
        changes: &[SpecChange],
        policies: Option<&str>,
    ) -> Result<SpecChangePlan, SpecChangeRefused> {
        let mut plan = SpecChangePlan::default();
        let mut refused = SpecChangeRefused::default();
        let staged;
        let cedar = match policies {
            Some(text) => {
                staged = AuthzEngine::empty();
                if let Err(error) = staged.reload_tenant_policies(tenant.as_str(), text) {
                    refused.reasons.push(format!(
                        "the tenant's new Cedar policies could not be loaded to check carries: \
                         {error}"
                    ));
                    return Err(refused);
                }
                &staged
            }
            None => self.authz.as_ref(),
        };
        for change in changes {
            let installed = self
                .registry
                .read()
                .expect("registry lock poisoned")
                .get_spec(tenant, &change.entity_type)
                .map(|spec| spec.table());
            let Some(old) = installed else {
                continue;
            };
            let new = TransitionTable::from_automaton(&change.proposed);
            let checked = Checked {
                tables: (&old, &new),
                cedar,
            };
            self.check_type(tenant, change, checked, &mut plan, &mut refused)
                .await;
        }
        if refused.is_empty() {
            Ok(plan)
        } else {
            Err(refused)
        }
    }

    /// Check one type's new version, adding its carries and warnings to
    /// `plan` and what stops it to `refused`.
    async fn check_type(
        &self,
        tenant: &TenantId,
        change: &SpecChange,
        checked: Checked<'_>,
        plan: &mut SpecChangePlan,
        refused: &mut SpecChangeRefused,
    ) {
        let (old, new) = checked.tables;
        let entity_type = change.entity_type.as_str();
        // Whether a stored record's moves keep a new invariant depends on
        // their effects and the caller's parameters, which the check does
        // not have; each such move is checked when it is made.
        for invariant in &new.invariants {
            if !old
                .invariants
                .iter()
                .any(|before| before.name == invariant.name && before.assert == invariant.assert)
            {
                plan.warnings.push(format!(
                    "{entity_type}: invariant '{}' is new or changed. Stored records are not \
                     checked against it; a move that would break it is refused when it is made",
                    invariant.name
                ));
            }
        }
        let carries = change.proposed.carry_moves();
        let statuses = affected_states(old, new, &carries);
        if statuses.is_empty() {
            return;
        }
        let names = read_names(old, new, &carries);
        let every_state: BTreeSet<String> = old
            .states
            .iter()
            .chain(&new.states)
            .chain(&statuses)
            .cloned()
            .collect();
        let (groups, unreadable) = match self
            .record_groups(tenant, entity_type, &names, &statuses, &every_state)
            .await
        {
            Ok(found) => found,
            Err(error) => {
                refused.reasons.push(format!(
                    "{entity_type}: its stored records could not be read to check the new \
                     version: {error}"
                ));
                return;
            }
        };
        if unreadable > 0 {
            plan.warnings.push(format!(
                "{entity_type}: {unreadable} stored records could not be read, so the new \
                 version is not checked against them"
            ));
        }
        let mut carried: BTreeMap<String, Carried<'_>> = BTreeMap::new();
        let mut stuck: BTreeMap<String, (Standing, Vec<String>)> = BTreeMap::new();
        let mut notes: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let waits_for = carry_moves(old);
        for group in groups {
            let before = old.stored_context(&group.values);
            let after = new.stored_context(&group.values);
            // A record a carry of the installed version selects waits for
            // it: the carry is its way forward, so a new version must carry
            // it too.
            let was = if waits_for
                .iter()
                .any(|action| old.blocked(&group.status, &before, action).is_none())
            {
                Standing::Free
            } else {
                old.standing(&group.status, &before)
            };
            let now = new.standing(&group.status, &after);
            let newly_stuck = match (&was, &now) {
                (_, Standing::Free | Standing::Rests) => {
                    if let Some(note) = answer::lost_moves(
                        entity_type,
                        (old, new),
                        &group.status,
                        (&before, &after),
                    ) {
                        notes.entry(note).or_default().extend(group.ids);
                    }
                    continue;
                }
                (Standing::Gone, Standing::Gone) => false,
                (_, Standing::Gone) => true,
                (_, Standing::Stuck(blocked)) => {
                    if let Some(action) = blocked
                        .iter()
                        .map(|(action, _)| action)
                        .find(|action| reads_related(new, action))
                    {
                        let note = format!(
                            "{entity_type} in {}: {action}'s guard reads a related record, so \
                             whether these records can still move is not checked",
                            group.status
                        );
                        notes.entry(note).or_default().extend(group.ids);
                        continue;
                    }
                    was == Standing::Free
                }
            };
            let carry = carries.iter().find(|(action, carry)| {
                carry.from == group.status && new.blocked(&group.status, &after, action).is_none()
            });
            if let Some((action, carry)) = carry {
                carried
                    .entry(action.clone())
                    .or_insert_with(|| Carried {
                        status: group.status.clone(),
                        action: action.clone(),
                        carry,
                        groups: Vec::new(),
                    })
                    .groups
                    .push(group.ids);
            } else if newly_stuck {
                stuck
                    .entry(group.status.clone())
                    .or_insert_with(|| (now, Vec::new()))
                    .1
                    .extend(group.ids);
            } else {
                let note = format!(
                    "{entity_type} in {}: these records had no way forward under the \
                     installed version either, and no [[carry]] moves them",
                    group.status
                );
                notes.entry(note).or_default().extend(group.ids);
            }
        }
        for (note, ids) in notes {
            plan.warnings.push(answer::with_records(&note, &ids));
        }
        for (status, (now, mut ids)) in stuck {
            ids.sort();
            refused.add_stuck(entity_type, (old, new), &status, &now, ids);
        }
        for (_, carried) in carried {
            self.try_carry(
                tenant,
                entity_type,
                (new, checked.cedar),
                carried,
                plan,
                refused,
            )
            .await;
        }
    }
}
