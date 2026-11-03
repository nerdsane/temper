//! What one delete removes, and whether it may go ahead.
//!
//! Deleting an entity follows every relation that points at its type, by that
//! relation's delete policy:
//! - `cascade`: the rows that reference it are removed with it, and the
//!   relations pointing at them are followed the same way;
//! - `restrict`: a row outside that set which references any entity in it
//!   refuses the whole delete;
//! - `none`: nothing happens, and the reference may dangle.
//!
//! The plan is made before anything is removed, so a refusal removes nothing.
//! The query index finds the rows that may reference an entity; a row joins
//! the plan only when its own state, read from its actor, still holds that
//! reference, and the plan keeps that state. The removal is one journal write
//! conditional on every planned state (`ServerState::delete_entities_together`),
//! so a row that changes after it is read, the requested entity included,
//! leaves the whole delete undone instead of being removed for a reference it
//! no longer holds. Each entity joins the set once, so a cycle of cascade
//! references ends where it meets a row already in it.

use std::collections::{BTreeMap, BTreeSet};

use temper_runtime::tenant::TenantId;
use temper_spec::cross_invariant::DeletePolicy;
use tracing::instrument;

use super::ConstraintViolation;
use super::delete_references::{ReferenceFinder, holds_id};
use crate::entity_actor::EntityState;
use crate::registry::RelationEdge;
use crate::state::ServerState;

/// One entity a delete removes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct DeleteTarget {
    pub entity_type: String,
    pub entity_id: String,
}

/// The entities one delete removes besides the requested one.
#[derive(Debug)]
pub struct DeletePlan {
    /// Each as the plan read it from its actor: when read, it held the
    /// reference that put it here. Distinct, and never the requested entity.
    dependents: Vec<EntityState>,
}

impl DeletePlan {
    /// The entities removed with the requested one, as they were read.
    pub fn dependents(&self) -> &[EntityState] {
        &self.dependents
    }

    /// Take the dependents' states for the delete that removes them.
    pub fn into_dependents(self) -> Vec<EntityState> {
        self.dependents
    }
}

/// Plan the delete of `entity_type('entity_id')` under its relations' delete
/// policies, or say why it may not go ahead.
///
/// A `restrict` reference is refused as `RelationIntegrity` (409). Rows that
/// could not be checked refuse the delete as `RelationUnverified` (503).
#[instrument(skip_all, fields(otel.name = "constraint.plan_delete", tenant = %tenant, entity_type, entity_id, operation, cascaded = tracing::field::Empty))]
pub async fn plan_delete(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
    operation: &str,
) -> Result<DeletePlan, ConstraintViolation> {
    let root = DeleteTarget {
        entity_type: entity_type.to_string(),
        entity_id: entity_id.to_string(),
    };
    let nothing_else = || DeletePlan {
        dependents: Vec::new(),
    };
    if !state.cross_invariant_enforce {
        state.metrics.record_cross_bypass();
        return Ok(nothing_else());
    }
    let incoming: BTreeMap<String, Vec<RelationEdge>> = {
        let registry = state.registry.read().unwrap();
        match registry.get_tenant(tenant) {
            Some(tc) => tc.relation_graph.incoming.clone(),
            None => return Ok(nothing_else()),
        }
    };
    let refusal = Refusal {
        state,
        tenant,
        root: &root,
        operation,
    };
    let mut finder = ReferenceFinder::new(state, tenant);

    let none_removed = BTreeSet::new();

    // Everything the delete removes, found by following cascade relations.
    // `removed` holds each entity once, so a row already in the set is never
    // found again: a cycle of cascade references ends there.
    let mut discovered = vec![root.clone()];
    let mut dependents = Vec::new();
    let mut removed: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    removed
        .entry(root.entity_type.clone())
        .or_default()
        .insert(root.entity_id.clone());
    let mut next = 0;
    while next < discovered.len() {
        let target = discovered[next].clone();
        next += 1;
        for edge in edges_with(&incoming, &target.entity_type, DeletePolicy::Cascade) {
            let already = removed.get(&edge.from_entity).unwrap_or(&none_removed);
            let ids = finder
                .referencing_ids(edge, &target.entity_id, already, usize::MAX)
                .await
                .map_err(|reason| refusal.unverified(&target, edge, &reason))?;
            // The cascade removes rows with the kernel's delete, which neither a
            // strict type nor one the Postgres actor runtime keeps takes. The
            // latter's rows are not read here, so any it may hold refuses.
            if let Some(source_id) = ids.first()
                && state.is_pg_actor_backed(tenant, &edge.from_entity)
            {
                return Err(refusal.not_removable(
                    edge,
                    source_id,
                    "which the Postgres actor runtime keeps outside the event journal",
                ));
            }
            for source_id in ids {
                // The index can hold a row's earlier fields, so the row's own
                // state decides whether it still belongs to `target`.
                let Some(row) = read_live_row(state, tenant, &edge.from_entity, &source_id)
                    .await
                    .map_err(|reason| refusal.unverified(&target, edge, &reason))?
                else {
                    continue;
                };
                if !holds_id(&row.fields, &edge.source_field, &target.entity_id) {
                    tracing::debug!(
                        from_entity = %edge.from_entity, source_id = %source_id,
                        "indexed reference no longer held; row stays"
                    );
                    continue;
                }
                if state.has_strict_action_params(tenant, &edge.from_entity) {
                    return Err(refusal.not_removable(
                        edge,
                        &source_id,
                        "which only a declared action deletes",
                    ));
                }
                removed
                    .entry(edge.from_entity.clone())
                    .or_default()
                    .insert(source_id.clone());
                discovered.push(DeleteTarget {
                    entity_type: edge.from_entity.clone(),
                    entity_id: source_id,
                });
                dependents.push(row);
            }
        }
    }
    debug_assert_eq!(dependents.len(), discovered.len() - 1);
    tracing::Span::current().record("cascaded", dependents.len());

    // No row the delete keeps may hold a restrict reference to one it removes.
    for target in &discovered {
        for edge in incoming.get(&target.entity_type).into_iter().flatten() {
            match edge.delete_policy {
                DeletePolicy::Cascade | DeletePolicy::None => {}
                DeletePolicy::SetNull => {
                    return Err(refusal.unverified(
                        target,
                        edge,
                        "set-null deletes are not implemented",
                    ));
                }
                DeletePolicy::Restrict => {
                    let skip = removed.get(&edge.from_entity).unwrap_or(&none_removed);
                    let found = finder
                        .referencing_ids(edge, &target.entity_id, skip, 1)
                        .await
                        .map_err(|reason| refusal.unverified(target, edge, &reason))?;
                    if let Some(source_id) = found.into_iter().next() {
                        return Err(refusal.referenced(target, edge, &source_id));
                    }
                }
            }
        }
    }

    Ok(DeletePlan { dependents })
}

/// The current state of a row the index named, or `None` once it is deleted.
async fn read_live_row(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
) -> Result<Option<EntityState>, String> {
    let response = state
        .get_tenant_entity_state(tenant, entity_type, entity_id)
        .await
        .map_err(|error| format!("{entity_type}('{entity_id}') could not be read: {error}"))?;
    if response.state.is_tombstoned() {
        // Reading it started an actor for a deleted row; it goes again.
        state.stop_and_remove_entity(tenant, entity_type, entity_id);
        return Ok(None);
    }
    Ok(Some(response.state))
}

/// The relations pointing at `entity_type` with `policy`.
fn edges_with<'a>(
    incoming: &'a BTreeMap<String, Vec<RelationEdge>>,
    entity_type: &str,
    policy: DeletePolicy,
) -> impl Iterator<Item = &'a RelationEdge> {
    incoming
        .get(entity_type)
        .into_iter()
        .flatten()
        .filter(move |edge| edge.delete_policy == policy)
}

/// Builds the violation that refuses one planned delete.
struct Refusal<'a> {
    state: &'a ServerState,
    tenant: &'a TenantId,
    root: &'a DeleteTarget,
    operation: &'a str,
}

impl Refusal<'_> {
    /// How the refusal names what it refuses: the requested entity, or the
    /// dependent of it that the delete would also remove.
    fn subject(&self, target: &DeleteTarget) -> String {
        let root = format!("{}('{}')", self.root.entity_type, self.root.entity_id);
        if target == self.root {
            root
        } else {
            format!(
                "{root}: it would delete {}('{}'), which",
                target.entity_type, target.entity_id
            )
        }
    }

    fn referenced(
        &self,
        target: &DeleteTarget,
        edge: &RelationEdge,
        source_id: &str,
    ) -> ConstraintViolation {
        tracing::warn!(
            tenant = %self.tenant, entity_type = %self.root.entity_type,
            entity_id = %self.root.entity_id, operation = self.operation,
            referenced_type = %target.entity_type, referenced_id = %target.entity_id,
            from_entity = %edge.from_entity, source_id,
            "constraint violation: cannot delete entity referenced by another"
        );
        self.state.metrics.record_relation_integrity_violation(
            self.tenant.as_str(),
            &self.root.entity_type,
            self.operation,
        );
        let verb = if target == self.root { ":" } else { " is" };
        ConstraintViolation::relation(
            format!(
                "cannot delete {}{verb} referenced by {}('{}') via {}",
                self.subject(target),
                edge.from_entity,
                source_id,
                edge.source_field
            ),
            &self.root.entity_type,
            &self.root.entity_id,
            self.operation,
        )
    }

    /// The cascade reaches a row the kernel's delete does not remove, for
    /// `why` (a clause naming what removes it instead).
    fn not_removable(
        &self,
        edge: &RelationEdge,
        source_id: &str,
        why: &str,
    ) -> ConstraintViolation {
        tracing::warn!(
            tenant = %self.tenant, entity_type = %self.root.entity_type,
            entity_id = %self.root.entity_id, operation = self.operation,
            from_entity = %edge.from_entity, source_id, why,
            "constraint violation: cascade reaches a type the kernel delete does not remove"
        );
        ConstraintViolation::relation(
            format!(
                "cannot delete {}('{}'): it would delete {}('{}'), {why}",
                self.root.entity_type, self.root.entity_id, edge.from_entity, source_id
            ),
            &self.root.entity_type,
            &self.root.entity_id,
            self.operation,
        )
    }

    fn unverified(
        &self,
        target: &DeleteTarget,
        edge: &RelationEdge,
        reason: &str,
    ) -> ConstraintViolation {
        tracing::warn!(
            tenant = %self.tenant, entity_type = %self.root.entity_type,
            entity_id = %self.root.entity_id, operation = self.operation,
            from_entity = %edge.from_entity, reason,
            "constraint check unavailable: cannot confirm what references the entity"
        );
        let verb = if target == self.root {
            ": could not"
        } else {
            " could not"
        };
        ConstraintViolation::relation_unverified(
            format!(
                "cannot delete {}{verb} check the {} rows that reference it via {}: {}",
                self.subject(target),
                edge.from_entity,
                edge.source_field,
                reason
            ),
            &self.root.entity_type,
            &self.root.entity_id,
            self.operation,
        )
    }
}
