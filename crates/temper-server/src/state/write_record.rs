//! The record a write will store, and the one constraint check every write
//! path runs on it.
//!
//! Relation integrity, field invariants and hard cross-invariants describe a
//! stored record. Each write path builds the record it is about to store with
//! the same function the entity actor applies for that write (create, field
//! update, action) and checks that record here, so the record that is checked
//! is the record that is stored. The checks read the record's values, never
//! how they are stored: a large value an earlier write stored out of line is
//! read back for the fields the checks read.
//!
//! Only writes from outside the kernel are checked. The kernel's own
//! follow-ons for a spec (reactions, state timers, schedules, WASM callbacks,
//! a WASM module's requests back into the API, platform code) must be able to
//! finish what an earlier move started, such as ending a session whose parent
//! was archived since.

use std::collections::BTreeSet;

use futures_util::FutureExt;
use serde_json::Value;
use temper_authz::SecurityContext;
use temper_jit::table::{RelatedMap, TransitionTable};
use temper_runtime::tenant::TenantId;
use temper_spec::cross_invariant::parse_related_field_assert;

use super::ServerState;

mod preview;
use crate::entity_actor::effects;
use crate::entity_actor::{EntityActor, EntityState};
use crate::odata::constraints::{self, ConstraintViolation, PendingRecords};
use crate::request_context::AgentContext;

/// Where a write came from. It decides whether the record the write stores
/// is checked before it is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteCaller {
    /// A call from outside the kernel: OData and MCP requests, HttpEndpoint
    /// routes and inbound webhooks.
    Outside,
    /// A follow-on the kernel runs for the spec: reactions, state timers,
    /// schedules, WASM callbacks and platform code, including a WASM module's
    /// own requests back into the kernel's API.
    Kernel,
}

impl WriteCaller {
    /// The caller behind an authenticated API request. A request a WASM
    /// module makes back into the kernel carries `context.module`, which the
    /// kernel sets when it hands the module its credential and which no
    /// outside credential resolves to; that request is the module's own work
    /// for the spec. Every other request comes from outside.
    pub(crate) fn of_request(security_ctx: &SecurityContext) -> Self {
        if security_ctx.context_attrs.contains_key("module") {
            Self::Kernel
        } else {
            Self::Outside
        }
    }
}

/// One write about to be applied: who makes it and what it stores into.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RecordWrite<'a> {
    /// Where the write came from.
    pub caller: WriteCaller,
    /// The tenant the record belongs to.
    pub tenant: &'a TenantId,
    /// The record's entity type.
    pub entity_type: &'a str,
    /// The record's id.
    pub entity_id: &'a str,
    /// The action, which selects the cross-invariants that apply (their `on`
    /// trigger), for example "Create" or "Patch".
    pub action: &'a str,
    /// The write's label in violations and metrics, for example "create".
    pub operation: &'a str,
}

/// An action about to be sent to the actor, with what it reads to build the
/// record it will store.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ActionPreview<'a> {
    /// The entity type's transition table.
    pub table: &'a TransitionTable,
    /// The record before the action.
    pub current: &'a EntityState,
    /// The action's name.
    pub action: &'a str,
    /// The action's params.
    pub params: &'a Value,
    /// Related records the action's guards read.
    pub related: &'a RelatedMap,
    /// The caller whose authority binds spawned initializers' trusted inputs.
    pub agent_ctx: &'a AgentContext,
}

/// Why an outside action cannot be checked or applied.
pub(crate) enum ActionRecordError {
    /// The record could not be checked or violates a declared constraint.
    Constraint(ConstraintViolation),
    /// The interpreter's own refusal, preserved without trying the action again.
    Preview(Box<effects::ProcessResult>),
}

impl From<ConstraintViolation> for ActionRecordError {
    fn from(violation: ConstraintViolation) -> Self {
        Self::Constraint(violation)
    }
}

impl ServerState {
    /// Check the record a write will store against the tenant's relation
    /// integrity, field invariants and hard cross-invariants. A kernel
    /// follow-on is not checked (see the module docs).
    pub(crate) async fn check_record_to_store(
        &self,
        write: RecordWrite<'_>,
        record: &Value,
    ) -> Result<(), ConstraintViolation> {
        self.check_record_with_pending(write, record, &PendingRecords::default())
            .await
    }

    /// [`Self::check_record_to_store`], with relation integrity and
    /// cross-invariants reading each record in `pending`, which the write
    /// brings into being after it commits, as it will then stand.
    async fn check_record_with_pending(
        &self,
        write: RecordWrite<'_>,
        record: &Value,
        pending: &PendingRecords,
    ) -> Result<(), ConstraintViolation> {
        let RecordWrite {
            caller,
            tenant,
            entity_type,
            entity_id,
            operation,
            ..
        } = write;
        if caller == WriteCaller::Kernel {
            return Ok(());
        }
        let resolved = self.stored_values_checked(write, record).await?;
        let record = resolved.as_ref().unwrap_or(record);
        constraints::pre_upsert_relation_checks(
            self,
            tenant,
            entity_type,
            entity_id,
            operation,
            record,
            pending,
        )
        .await?;
        constraints::pre_upsert_field_invariant_checks(
            self,
            tenant,
            entity_type,
            entity_id,
            operation,
            record,
        )
        .await?;
        constraints::post_write_invariant_checks(self, write, record, pending).await
    }

    /// `record` with each field its checks read holding its value, when an
    /// earlier write stored one of them out of line (a field-overflow
    /// reference, carried unchanged into the record this write stores): the
    /// value is read back from the tenant's blob store, so the checks read
    /// the domain value whatever its size. `None` when no field the checks
    /// read is stored that way. Values this write brings are already in the
    /// record (a blob-backed action's preview keeps new values inline).
    async fn stored_values_checked(
        &self,
        write: RecordWrite<'_>,
        record: &Value,
    ) -> Result<Option<Value>, ConstraintViolation> {
        let RecordWrite {
            tenant,
            entity_type,
            entity_id,
            action,
            operation,
            ..
        } = write;
        let names = self.fields_checked(tenant, entity_type, action);
        let source = crate::blobs::BlobReadSource::Tenant {
            state: self,
            tenant,
        };
        // Boxed: the blob read's future nested in every write path is too
        // deep for their callers' `Send` checks.
        crate::blobs::resolve_stored_fields(&source, record, &names)
            .boxed()
            .await
            .map_err(|error| {
                tracing::warn!(
                    tenant = %tenant, entity_type, entity_id, operation, %error,
                    "constraint check: a stored value it reads could not be read"
                );
                ConstraintViolation::stored_value_unreadable(entity_type, entity_id, operation)
            })
    }

    /// The fields of an `entity_type` record that the checks on `action`
    /// read: the names its field invariants read, its relation fields, and
    /// the source fields of the cross-invariants the action triggers.
    fn fields_checked(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        action: &str,
    ) -> BTreeSet<String> {
        let mut names = BTreeSet::new();
        if !self.cross_invariant_enforce {
            return names;
        }
        let registry = self.registry.read().expect("registry lock poisoned");
        for invariant in registry
            .field_invariants_for(tenant, entity_type)
            .unwrap_or_default()
        {
            names.extend(invariant.referenced_fields());
        }
        let Some(tenant_config) = registry.get_tenant(tenant) else {
            return names;
        };
        if let Some(edges) = tenant_config.relation_graph.outgoing.get(entity_type) {
            names.extend(edges.iter().map(|edge| edge.source_field.clone()));
        }
        if let Some(spec) = &tenant_config.cross_invariants {
            names.extend(
                spec.invariants
                    .iter()
                    .filter(|inv| constraints::trigger_matches(&inv.on, entity_type, action))
                    .filter_map(|inv| parse_related_field_assert(&inv.assertion))
                    .map(|assertion| assertion.source_field),
            );
        }
        names
    }

    /// Check the record `action` will store when applied to `current`.
    ///
    /// Runs before the action is sent to the actor, which the caller then
    /// holds to `current`: the actor applies the action only while the record
    /// is still `current`, so the record checked here is the record stored.
    /// Kernel follow-ons and entity types with no relation, field invariant
    /// or matching cross-invariant skip the work.
    pub(crate) async fn check_action_record(
        &self,
        caller: WriteCaller,
        tenant: &TenantId,
        preview: ActionPreview<'_>,
    ) -> Result<(), ActionRecordError> {
        let ActionPreview {
            current, action, ..
        } = preview;
        if !self.action_record_checked(caller, tenant, &current.entity_type, action) {
            return Ok(());
        }
        // Boxed as a trait object: the preview runs the action interpreter,
        // and its future type nested in every dispatch path is too deep for
        // their callers' `Send` checks.
        let (record, pending) = self.action_record(tenant, preview).boxed().await?;
        self.check_record_with_pending(
            RecordWrite {
                caller,
                tenant,
                entity_type: &current.entity_type,
                entity_id: &current.entity_id,
                action,
                operation: "action",
            },
            &record,
            &pending,
        )
        .await
        .map_err(ActionRecordError::from)
    }

    /// Whether the record a `caller`'s `action` on `entity_type` stores is
    /// checked: the call comes from outside the kernel and the tenant declares
    /// a constraint that write must satisfy.
    pub(crate) fn action_record_checked(
        &self,
        caller: WriteCaller,
        tenant: &TenantId,
        entity_type: &str,
        action: &str,
    ) -> bool {
        if caller == WriteCaller::Kernel || !self.cross_invariant_enforce {
            return false;
        }
        let registry = self.registry.read().expect("registry lock poisoned");
        let Some(tenant_config) = registry.get_tenant(tenant) else {
            return false;
        };
        let has_relations = tenant_config
            .relation_graph
            .outgoing
            .get(entity_type)
            .is_some_and(|edges| !edges.is_empty());
        let has_cross_invariants = tenant_config.cross_invariants.as_ref().is_some_and(|spec| {
            spec.invariants
                .iter()
                .any(|inv| constraints::trigger_matches(&inv.on, entity_type, action))
        });
        let has_field_invariants = registry
            .field_invariants_for(tenant, entity_type)
            .is_some_and(|invariants| !invariants.is_empty());
        has_relations || has_cross_invariants || has_field_invariants
    }
}

/// The fields a create stores: the initial fields as the actor builds its
/// first state from them.
pub(crate) fn created_record(
    entity_type: &str,
    entity_id: &str,
    table: &TransitionTable,
    initial_fields: &Value,
) -> Value {
    EntityActor::build_initial_state(entity_type, entity_id, table, initial_fields).fields
}

/// The fields a field update stores: a PATCH merge (`replace == false`) or a
/// PUT replacement, applied to `current` by the actor's own field-update
/// function.
pub(crate) fn field_update_record(current: &EntityState, body: &Value, replace: bool) -> Value {
    let body = effects::sanitize_action_params(body);
    let mut next = current.clone();
    if effects::apply_field_update(&mut next, &body, replace) {
        next.fields
    } else {
        current.fields.clone()
    }
}
