use super::*;

use std::collections::{BTreeMap, BTreeSet};

use temper_runtime::persistence::schema_deployment::{
    CompleteSchemaBootstrap, RecordSchemaBootstrapActionFailure, RecordSchemaBootstrapCreated,
    ReserveSchemaBootstrap, ReserveSchemaBootstrapOutcome, SchemaBootstrapAction,
    SchemaBootstrapFailure, SchemaBootstrapFailureStage, SchemaBootstrapOperation,
    SchemaBootstrapReceipt, SchemaBootstrapStatus,
};
use temper_wasm_sdk::data::{
    DataOperationKind, EntityDataGrant, ManifestValueSourceV1, ModuleDataGrant,
};
use temper_wasm_sdk::schema_deployment::{
    BootstrapDispatchReceiptV1, BootstrapDispatchRequestV1, BootstrapFailureStageV1,
    BootstrapFailureV1, BootstrapSchemaPinV1,
};

use crate::application_data::{
    GovernedApplicationDataService, canonical_manifest_entity_value_from_parts,
    validate_manifest_action_params, validate_manifest_entity_object,
};
use crate::entity_actor::SCHEMA_BOOTSTRAP_ACTION_IDEMPOTENCY_PREFIX;
use crate::request_context::AgentContext;

mod authorization;
mod support;
#[cfg(test)]
mod tests;

pub(crate) use support::BootstrapInvocationIdentity;
use support::{
    BootstrapAcceptedAuthority, bound_receipt_for_invocation, caller_authority_digest,
    module_error_failure, receipt_v1, runtime_type, service_error_failure, simple_failure,
    validate_request, validation_entity,
};

const BOOTSTRAP_ACTION: &str = "schema_bootstrap_dispatch";

impl GovernedSchemaDeploymentService<'_> {
    pub(crate) async fn bootstrap_dispatch(
        &self,
        tenant: &str,
        security: &SecurityContext,
        invocation: BootstrapInvocationIdentity,
        request: BootstrapDispatchRequestV1,
    ) -> Result<BootstrapDispatchReceiptV1, ServiceError> {
        validate_request(&request)?;
        let caller_authority = caller_authority_digest(security, &invocation)?;
        let canonical_initial_fields_json =
            canonical_json_object(&serde_json::Value::Object(request.initial_fields.clone()))?;
        let canonical_parameters_json = request
            .initial_action
            .as_ref()
            .map(|action| {
                canonical_json_object(&serde_json::Value::Object(action.parameters.clone()))
            })
            .transpose()?;
        let request_digest = digest_json(&(
            "schema_bootstrap_dispatch/v1",
            tenant,
            caller_authority.as_str(),
            request.activation_request_id.as_str(),
            request.entity_type.as_str(),
            request.entity_id.as_str(),
            canonical_initial_fields_json.as_str(),
            request
                .initial_action
                .as_ref()
                .map(|action| action.action.as_str()),
            canonical_parameters_json.as_deref(),
        ))?;
        let accepted_authority_json = serde_json::to_string(&BootstrapAcceptedAuthority {
            security: security.clone(),
            invocation,
        })
        .map_err(|error| ServiceError::new("invalid_bootstrap", error.to_string(), false))?;
        let initial_action = request.initial_action.map(|action| SchemaBootstrapAction {
            action: action.action,
            canonical_parameters_json: canonical_parameters_json
                .expect("action parameters were canonicalized"),
            idempotency_key: format!(
                "{SCHEMA_BOOTSTRAP_ACTION_IDEMPOTENCY_PREFIX}{request_digest}"
            ),
        });
        let outcome = self
            .store()?
            .reserve_schema_bootstrap(ReserveSchemaBootstrap {
                tenant: tenant.to_string(),
                caller_authority,
                accepted_authority_json,
                idempotency_key: request.idempotency_key,
                request_digest: request_digest.clone(),
                request_id: request.request_id,
                activation_request_id: request.activation_request_id,
                entity_type: request.entity_type,
                entity_id: request.entity_id,
                canonical_initial_fields_json,
                initial_action,
            })
            .await
            .map_err(ServiceError::from_store)?;
        let operation = match outcome {
            ReserveSchemaBootstrapOutcome::Reserved(operation)
            | ReserveSchemaBootstrapOutcome::Replayed(operation) => operation,
        };
        self.drive_bootstrap(operation).await
    }

    pub(crate) async fn drive_bootstrap(
        &self,
        mut operation: SchemaBootstrapOperation,
    ) -> Result<BootstrapDispatchReceiptV1, ServiceError> {
        if let Some(receipt) = operation.receipt.as_ref() {
            return receipt_v1(receipt);
        }
        let accepted: BootstrapAcceptedAuthority =
            serde_json::from_str(&operation.command.accepted_authority_json).map_err(|error| {
                ServiceError::new("backend_unavailable", error.to_string(), true)
            })?;
        let record = self
            .store()?
            .get_schema_deployment(
                &operation.command.tenant,
                &operation.pin.scope,
                &operation.pin.bundle_digest,
            )
            .await
            .map_err(ServiceError::from_store)?
            .ok_or_else(|| {
                ServiceError::new(
                    "backend_unavailable",
                    "bootstrap reservation lost its immutable bundle",
                    true,
                )
            })?;
        let entity = match validation_entity(&record, &operation) {
            Ok(entity) => entity,
            Err(error) => {
                return self
                    .complete_bootstrap_failure(
                        operation,
                        module_error_failure(SchemaBootstrapFailureStage::Validation, error),
                    )
                    .await;
            }
        };
        if let Err(error) = self
            .authorize_bootstrap(&operation, &accepted.security, &accepted.invocation)
            .await
        {
            let failure = service_error_failure(SchemaBootstrapFailureStage::Authorization, &error);
            return self.complete_bootstrap_failure(operation, failure).await;
        }
        self.recover_registry_bundle(
            &operation.command.tenant,
            &operation.pin.scope,
            &operation.pin.bundle_digest,
        )
        .await?;
        self.recover_registry_pointer(&operation.command.tenant, &operation.pin.scope)
            .await?;

        let tenant_id = TenantId::new(&operation.command.tenant);
        let mut initial_fields: serde_json::Map<String, serde_json::Value> = serde_json::from_str(
            &operation.command.canonical_initial_fields_json,
        )
        .map_err(|error| ServiceError::new("backend_unavailable", error.to_string(), true))?;
        for property in &entity.properties {
            if property.source == ManifestValueSourceV1::EntityId {
                if initial_fields
                    .get(&property.canonical_name)
                    .is_some_and(|value| {
                        value.as_str() != Some(operation.command.entity_id.as_str())
                    })
                {
                    return self
                        .complete_bootstrap_failure(
                            operation,
                            simple_failure(
                                SchemaBootstrapFailureStage::Validation,
                                "entity_id_mismatch",
                                "initial entity identity does not match the bootstrap target",
                                false,
                            ),
                        )
                        .await;
                }
                initial_fields.insert(
                    property.canonical_name.clone(),
                    operation.command.entity_id.clone().into(),
                );
            }
        }
        let creation_key = format!(
            "schema-bootstrap-create:{}",
            operation.command.request_digest
        );
        let creation = match self
            .state
            .get_or_create_scoped_entity_for_bootstrap(
                &tenant_id,
                runtime_type(&operation.command.entity_type),
                &operation.command.entity_id,
                serde_json::Value::Object(initial_fields),
                operation.pin.clone(),
                creation_key,
            )
            .await
        {
            Ok(creation) => creation,
            Err(error) if error.starts_with("BootstrapTargetConflict:") => {
                return self
                    .complete_bootstrap_failure(
                        operation,
                        simple_failure(
                            SchemaBootstrapFailureStage::Conflict,
                            "bootstrap_target_exists",
                            "the scoped entity journal is owned by another creation",
                            false,
                        ),
                    )
                    .await;
            }
            Err(error) => {
                return Err(ServiceError::new("bootstrap_creation_pending", error, true));
            }
        };
        if operation.status == SchemaBootstrapStatus::Reserved {
            operation = self
                .store()?
                .record_schema_bootstrap_created(RecordSchemaBootstrapCreated {
                    tenant: operation.command.tenant.clone(),
                    caller_authority: operation.command.caller_authority.clone(),
                    idempotency_key: operation.command.idempotency_key.clone(),
                    expected_sequence: operation.committed_sequence,
                    creation_sequence: creation.creation_sequence,
                })
                .await
                .map_err(ServiceError::from_store)?;
        }
        if let Some(receipt) = operation.receipt.as_ref() {
            return receipt_v1(receipt);
        }

        let mut receipt = SchemaBootstrapReceipt {
            request_id: operation.command.request_id.clone(),
            pin: operation.pin.clone(),
            entity_type: operation.command.entity_type.clone(),
            entity_id: operation.command.entity_id.clone(),
            creation_sequence: operation.creation_sequence,
            action_sequence: None,
            canonical_action_result_json: None,
            failure: None,
        };
        if let Some(action) = operation.command.initial_action.clone() {
            const ACTION_REPLAY_BUDGET: usize = 10_000;
            if let Some(failure) = operation.action_failure.clone() {
                receipt.failure = Some(failure);
            } else {
                let mut journal_outcome = self
                    .state
                    .scoped_bootstrap_action_outcome(
                        &tenant_id,
                        runtime_type(&operation.command.entity_type),
                        &operation.command.entity_id,
                        &operation.pin,
                        &action.idempotency_key,
                        &action.action,
                        &action.canonical_parameters_json,
                        ACTION_REPLAY_BUDGET,
                    )
                    .await
                    .map_err(|error| ServiceError::new("bootstrap_action_pending", error, true))?;
                if journal_outcome.is_none() {
                    let params = serde_json::from_str(&action.canonical_parameters_json).map_err(
                        |error| ServiceError::new("backend_unavailable", error.to_string(), true),
                    )?;
                    let agent = AgentContext {
                        security_ctx: Some(accepted.security.clone()),
                        agent_id: Some(accepted.security.principal.id.clone()),
                        idempotency_key: Some(action.idempotency_key.clone()),
                        schema_pin: Some(operation.pin.clone()),
                        ..AgentContext::default()
                    };
                    let response = GovernedApplicationDataService::new(self.state)
                        .action_for_schema_bootstrap(
                            &tenant_id,
                            runtime_type(&operation.command.entity_type),
                            &operation.command.entity_id,
                            &action.action,
                            params,
                            &agent,
                        )
                        .await
                        .map_err(|error| {
                            ServiceError::new("bootstrap_action_pending", error, true)
                        })?;
                    if !response.success {
                        let failure = simple_failure(
                            SchemaBootstrapFailureStage::Action,
                            "action_rejected",
                            response
                                .error
                                .as_deref()
                                .unwrap_or("initial action rejected"),
                            false,
                        );
                        operation = self
                            .store()?
                            .record_schema_bootstrap_action_failure(
                                RecordSchemaBootstrapActionFailure {
                                    tenant: operation.command.tenant.clone(),
                                    caller_authority: operation.command.caller_authority.clone(),
                                    idempotency_key: operation.command.idempotency_key.clone(),
                                    expected_sequence: operation.committed_sequence,
                                    failure: failure.clone(),
                                },
                            )
                            .await
                            .map_err(ServiceError::from_store)?;
                        if let Some(completed) = operation.receipt.as_ref() {
                            return receipt_v1(completed);
                        }
                        receipt.failure = operation.action_failure.clone().or(Some(failure));
                    } else {
                        journal_outcome = self
                            .state
                            .scoped_bootstrap_action_outcome(
                                &tenant_id,
                                runtime_type(&operation.command.entity_type),
                                &operation.command.entity_id,
                                &operation.pin,
                                &action.idempotency_key,
                                &action.action,
                                &action.canonical_parameters_json,
                                ACTION_REPLAY_BUDGET,
                            )
                            .await
                            .map_err(|error| {
                                ServiceError::new("bootstrap_action_pending", error, true)
                            })?;
                        if journal_outcome.is_none() {
                            return Err(ServiceError::new(
                                "bootstrap_action_pending",
                                "committed bootstrap action has no durable journal outcome",
                                true,
                            ));
                        }
                    }
                }
                if let Some(outcome) = journal_outcome {
                    receipt.action_sequence = Some(outcome.sequence);
                    let action_result = entity
                        .actions
                        .iter()
                        .find(|candidate| candidate.canonical_name == action.action)
                        .and_then(|manifest_action| manifest_action.result_type.as_deref())
                        .filter(|result_type| *result_type == entity.entity_type)
                        .map(|_| {
                            canonical_manifest_entity_value_from_parts(
                                &entity,
                                &operation.command.entity_id,
                                &outcome.status,
                                &outcome.fields,
                            )
                        })
                        .transpose()
                        .map_err(|error| {
                            ServiceError::new("bootstrap_action_pending", error.message, true)
                        })?
                        .map(serde_json::Value::Object)
                        .unwrap_or(outcome.fields);
                    receipt.canonical_action_result_json =
                        Some(canonical_json_object(&action_result)?);
                }
            }
        }
        let receipt =
            bound_receipt_for_invocation(receipt, accepted.invocation.max_response_bytes)?;
        let completed = self
            .store()?
            .complete_schema_bootstrap(CompleteSchemaBootstrap {
                tenant: operation.command.tenant.clone(),
                caller_authority: operation.command.caller_authority.clone(),
                idempotency_key: operation.command.idempotency_key.clone(),
                expected_sequence: operation.committed_sequence,
                receipt,
            })
            .await
            .map_err(ServiceError::from_store)?;
        receipt_v1(
            completed
                .receipt
                .as_ref()
                .expect("completed bootstrap has receipt"),
        )
    }

    async fn complete_bootstrap_failure(
        &self,
        operation: SchemaBootstrapOperation,
        failure: SchemaBootstrapFailure,
    ) -> Result<BootstrapDispatchReceiptV1, ServiceError> {
        let accepted: BootstrapAcceptedAuthority =
            serde_json::from_str(&operation.command.accepted_authority_json).map_err(|error| {
                ServiceError::new("backend_unavailable", error.to_string(), true)
            })?;
        let receipt = bound_receipt_for_invocation(
            SchemaBootstrapReceipt {
                request_id: operation.command.request_id.clone(),
                pin: operation.pin.clone(),
                entity_type: operation.command.entity_type.clone(),
                entity_id: operation.command.entity_id.clone(),
                creation_sequence: operation.creation_sequence,
                action_sequence: None,
                canonical_action_result_json: None,
                failure: Some(failure),
            },
            accepted.invocation.max_response_bytes,
        )?;
        let completed = self
            .store()?
            .complete_schema_bootstrap(CompleteSchemaBootstrap {
                tenant: operation.command.tenant.clone(),
                caller_authority: operation.command.caller_authority.clone(),
                idempotency_key: operation.command.idempotency_key.clone(),
                expected_sequence: operation.committed_sequence,
                receipt,
            })
            .await
            .map_err(ServiceError::from_store)?;
        receipt_v1(
            completed
                .receipt
                .as_ref()
                .expect("completed bootstrap has receipt"),
        )
    }
}
