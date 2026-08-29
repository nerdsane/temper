//! Source-action integration for the bounded collection workflow runtime.

use temper_jit::table::TransitionTable;
use temper_runtime::persistence::schema_deployment::SchemaEventPin;
use temper_runtime::persistence::{PersistenceAppend, PersistenceError};

use super::EntityState;
use crate::storage::BoxedEventStore;
use crate::trigger::collection_workflow::{
    CollectionExecutionActions, CollectionLedgerCommitOutcome, CollectionRequestedOutcome,
    CollectionWorkflowBudgets, CollectionWorkflowRecordV1, CollectionWorkflowStart,
    commit_activated_start, commit_controlled, load_active_source_workflow_id,
    load_collection_record,
};
use crate::trigger::delivery::ReactionReceipt;

/// Commit a source start/control action through the collection ledger when declared.
#[expect(
    clippy::too_many_arguments,
    reason = "collection commit identity remains explicit"
)]
pub(super) async fn commit_collection_source_action(
    store: &BoxedEventStore,
    source_append: PersistenceAppend,
    table: &TransitionTable,
    state: &EntityState,
    tenant: &str,
    entity_type: &str,
    entity_id: &str,
    action: &str,
    authority: Option<&serde_json::Value>,
    schema_pin: Option<SchemaEventPin>,
    receipt: Option<&ReactionReceipt>,
) -> Result<Option<u64>, PersistenceError> {
    let Some((declaration, role)) = table.collection_workflows.iter().find_map(|workflow| {
        if workflow.start_action == action {
            Some((workflow, SourceRole::Start))
        } else if workflow.cancel_action == action {
            Some((workflow, SourceRole::Cancel))
        } else if workflow.timeout_action == action {
            Some((workflow, SourceRole::Timeout))
        } else {
            None
        }
    }) else {
        return Ok(None);
    };
    let authority = authority.cloned().ok_or_else(|| {
        PersistenceError::Serialization(
            "collection source action is missing its committed authority".to_string(),
        )
    })?;
    let source_sequence = source_append.expected_sequence + 1;
    let source_fence_append =
        if let Some(receipt) = receipt.filter(|receipt| receipt.collection.is_some()) {
            Some(
                crate::trigger::collection_workflow::target_fence_append(store, tenant, receipt)
                    .await
                    .map_err(PersistenceError::Storage)?,
            )
        } else {
            None
        };
    match role {
        SourceRole::Start => {
            let roster = state
                .lists
                .get(&declaration.roster_field)
                .cloned()
                .ok_or_else(|| {
                    PersistenceError::Serialization(format!(
                        "collection roster '{}' is absent from committed source state",
                        declaration.roster_field
                    ))
                })?;
            let schema_digest = table.schema_digest.clone().ok_or_else(|| {
                PersistenceError::Serialization(
                    "collection source table has no verified schema digest".to_string(),
                )
            })?;
            let (intent, mut record) = CollectionWorkflowRecordV1::start(CollectionWorkflowStart {
                tenant: tenant.to_string(),
                source_entity_type: entity_type.to_string(),
                source_entity_id: entity_id.to_string(),
                declaration_name: declaration.name.clone(),
                source_action: action.to_string(),
                source_sequence,
                schema_digest,
                schema_pin,
                authority,
                roster,
                budgets: CollectionWorkflowBudgets {
                    max_members: declaration.max_members,
                    max_concurrency: declaration.max_concurrency,
                    max_attempts: declaration.max_attempts,
                },
            })
            .map_err(PersistenceError::Serialization)?;
            let actions = execution_actions(declaration);
            let outcome = commit_activated_start(
                store,
                source_append,
                &intent,
                &mut record,
                &actions,
                source_fence_append,
            )
            .await
            .map_err(PersistenceError::Storage)?;
            Ok(Some(source_sequence_from(outcome)?))
        }
        SourceRole::Cancel | SourceRole::Timeout => {
            let workflow_id = load_active_source_workflow_id(
                store,
                tenant,
                entity_type,
                entity_id,
                &declaration.name,
                schema_pin.as_ref(),
            )
            .await?
            .ok_or_else(|| {
                PersistenceError::Serialization(
                    "collection control has no active workflow".to_string(),
                )
            })?;
            let (mut record, workflow_sequence) =
                load_collection_record(store, tenant, &workflow_id)
                    .await?
                    .ok_or_else(|| {
                        PersistenceError::Serialization(
                            "active collection workflow journal is missing".to_string(),
                        )
                    })?;
            let requested = match role {
                SourceRole::Cancel => CollectionRequestedOutcome::Cancelled,
                SourceRole::Timeout => CollectionRequestedOutcome::TimedOut,
                SourceRole::Start => unreachable!("start handled above"),
            };
            let timeout_delivery_id = (role == SourceRole::Timeout)
                .then(|| receipt.map(|receipt| receipt.delivery_id.as_str()))
                .flatten();
            let (intent, _) = record
                .request_control(
                    requested,
                    timeout_delivery_id,
                    action.to_string(),
                    source_sequence,
                    authority,
                    schema_pin,
                )
                .map_err(PersistenceError::Serialization)?;
            let outcome = commit_controlled(
                store,
                source_append,
                &intent,
                workflow_sequence,
                &mut record,
                source_fence_append,
            )
            .await
            .map_err(PersistenceError::Storage)?;
            Ok(Some(source_sequence_from(outcome)?))
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SourceRole {
    Start,
    Cancel,
    Timeout,
}

fn execution_actions(
    declaration: &temper_spec::automaton::CollectionWorkflow,
) -> CollectionExecutionActions<'_> {
    CollectionExecutionActions {
        member_entity: &declaration.member_entity,
        member_action: &declaration.member_action,
        member_cancel_action: &declaration.member_cancel_action,
        timeout_action: &declaration.timeout_action,
        on_success: &declaration.on_success,
        on_partial_failure: &declaration.on_partial_failure,
        on_failure: &declaration.on_failure,
        on_cancelled: &declaration.on_cancelled,
        on_timed_out: &declaration.on_timed_out,
    }
}

fn source_sequence_from(outcome: CollectionLedgerCommitOutcome) -> Result<u64, PersistenceError> {
    let results = match outcome {
        CollectionLedgerCommitOutcome::Committed(results)
        | CollectionLedgerCommitOutcome::Reconciled(results) => results,
    };
    results
        .first()
        .map(|result| result.sequence_nr)
        .ok_or_else(|| {
            PersistenceError::Storage("collection commit returned no source result".to_string())
        })
}
