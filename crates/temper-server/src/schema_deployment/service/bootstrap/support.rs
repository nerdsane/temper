use super::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct BootstrapInvocationIdentity {
    /// Host-verified module name.
    pub(crate) module_name: String,
    /// Host-verified immutable module artifact digest.
    pub(crate) artifact_digest: String,
    /// Host-verified grant digest.
    pub(crate) grant_digest: String,
    /// Host-verified invocation trigger.
    pub(crate) trigger: String,
    /// Maximum encoded response size accepted by the invocation grant.
    pub(crate) max_response_bytes: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct BootstrapAcceptedAuthority {
    pub(super) security: SecurityContext,
    pub(super) invocation: BootstrapInvocationIdentity,
}

pub(super) fn validate_request(request: &BootstrapDispatchRequestV1) -> Result<(), ServiceError> {
    for (name, value) in [
        ("request id", request.request_id.as_str()),
        ("idempotency key", request.idempotency_key.as_str()),
        (
            "activation request id",
            request.activation_request_id.as_str(),
        ),
        ("entity type", request.entity_type.as_str()),
        ("entity id", request.entity_id.as_str()),
    ] {
        if value.trim().is_empty() || value.trim() != value || value.len() > 256 {
            return Err(ServiceError::new(
                "invalid_bootstrap",
                format!("{name} must be canonical and at most 256 bytes"),
                false,
            ));
        }
    }
    if request.initial_fields.len() > 1_024
        || request
            .initial_action
            .as_ref()
            .is_some_and(|action| action.parameters.len() > 1_024)
    {
        return Err(ServiceError::new(
            "bootstrap_budget_exhausted",
            "bootstrap field or parameter item budget exhausted",
            false,
        ));
    }
    Ok(())
}

pub(super) fn caller_authority_digest(
    security: &SecurityContext,
    invocation: &BootstrapInvocationIdentity,
) -> Result<String, ServiceError> {
    let principal_attributes = security
        .principal
        .attributes
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<BTreeMap<_, _>>();
    let context_attributes = security
        .context_attrs
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<BTreeMap<_, _>>();
    digest_json(&(
        "schema-bootstrap-caller/v1",
        &security.principal.id,
        &security.principal.kind,
        &security.principal.role,
        &security.principal.acting_for,
        &security.principal.agent_type,
        principal_attributes,
        context_attributes,
        &invocation.module_name,
        &invocation.artifact_digest,
        &invocation.grant_digest,
        &invocation.trigger,
        invocation.max_response_bytes,
    ))
}

pub(super) fn bound_receipt_for_invocation(
    mut receipt: SchemaBootstrapReceipt,
    max_response_bytes: u32,
) -> Result<SchemaBootstrapReceipt, ServiceError> {
    let encoded_len = |candidate: &SchemaBootstrapReceipt| -> Result<usize, ServiceError> {
        let response = temper_wasm_sdk::schema_deployment::SchemaDeploymentResponseV1::Bootstrap {
            receipt: receipt_v1(candidate)?,
        };
        serde_json::to_vec(&response)
            .map(|bytes| bytes.len())
            .map_err(|error| ServiceError::new("backend_unavailable", error.to_string(), true))
    };
    if encoded_len(&receipt)? <= max_response_bytes as usize {
        return Ok(receipt);
    }
    receipt.canonical_action_result_json = None;
    receipt.failure = Some(simple_failure(
        SchemaBootstrapFailureStage::Budget,
        "response_budget_exhausted",
        "bootstrap result exceeded the invocation response budget",
        false,
    ));
    Ok(receipt)
}

pub(super) fn validation_entity(
    record: &SchemaDeploymentRecord,
    operation: &SchemaBootstrapOperation,
) -> Result<temper_wasm_sdk::data::ManifestEntityV1, temper_wasm_sdk::data::ModuleDataError> {
    let csdl = temper_spec::parse_csdl(&record.bundle.canonical_csdl).map_err(|error| {
        validation_error(
            temper_wasm_sdk::data::ModuleDataErrorKind::SchemaMismatch,
            "InvalidCanonicalCsdl",
            error.to_string(),
        )
    })?;
    let mut operations = BTreeSet::from([DataOperationKind::EntityCreate]);
    let mut actions = BTreeSet::new();
    if let Some(action) = operation.command.initial_action.as_ref() {
        operations.insert(DataOperationKind::ActionInvoke);
        actions.insert(action.action.clone());
    }
    let grant = ModuleDataGrant {
        operations,
        entities: vec![EntityDataGrant {
            entity_type: operation.command.entity_type.clone(),
            actions,
            ..EntityDataGrant::default()
        }],
        ..ModuleDataGrant::default()
    };
    let ioa = record
        .bundle
        .canonical_ioa
        .iter()
        .map(|(entity_type, source)| IoaSourceInput {
            entity_type: entity_type.clone(),
            source: source.clone(),
        })
        .collect::<Vec<_>>();
    let generated = temper_codegen::generate_module_sdk(
        &csdl,
        &ioa,
        "schema-bootstrap-validator",
        &operation.pin.bundle_digest,
        &operation.pin.bundle_digest,
        &operation.pin.bundle_digest,
        grant,
    )
    .map_err(|error| {
        validation_error(
            temper_wasm_sdk::data::ModuleDataErrorKind::SchemaMismatch,
            "BootstrapClosureMismatch",
            error.to_string(),
        )
    })?;
    let entity = generated
        .manifest
        .entities
        .iter()
        .find(|entity| entity.entity_type == operation.command.entity_type)
        .ok_or_else(|| {
            validation_error(
                temper_wasm_sdk::data::ModuleDataErrorKind::SchemaMismatch,
                "UnknownEntityType",
                "entity type is absent from the exact bundle closure",
            )
        })?;
    let fields = serde_json::from_str(&operation.command.canonical_initial_fields_json).map_err(
        |error| {
            validation_error(
                temper_wasm_sdk::data::ModuleDataErrorKind::InvalidRequest,
                "InvalidInitialFields",
                error.to_string(),
            )
        },
    )?;
    validate_manifest_entity_object(entity, &fields, true)?;
    if let Some(action) = operation.command.initial_action.as_ref() {
        let params = serde_json::from_str(&action.canonical_parameters_json).map_err(|error| {
            validation_error(
                temper_wasm_sdk::data::ModuleDataErrorKind::InvalidRequest,
                "InvalidActionParameters",
                error.to_string(),
            )
        })?;
        validate_manifest_action_params(&csdl, entity, &action.action, &params)?;
    }
    Ok(entity.clone())
}

pub(super) fn simple_failure(
    stage: SchemaBootstrapFailureStage,
    code: &str,
    message: &str,
    retryable: bool,
) -> SchemaBootstrapFailure {
    SchemaBootstrapFailure {
        stage,
        code: code.chars().take(128).collect(),
        message: message.chars().take(1_024).collect(),
        retryable,
        decision_id: None,
        details: BTreeMap::new(),
    }
}

pub(super) fn module_error_failure(
    stage: SchemaBootstrapFailureStage,
    error: temper_wasm_sdk::data::ModuleDataError,
) -> SchemaBootstrapFailure {
    let mut failure = simple_failure(stage, &error.code, &error.message, false);
    failure.decision_id = error.decision_id;
    failure.details = error.details.map_or_else(BTreeMap::new, |details| {
        details
            .into_iter()
            .take(64)
            .map(|(key, value)| {
                (
                    key.chars().take(128).collect(),
                    bounded_detail_value(value, 0),
                )
            })
            .collect()
    });
    failure
}

fn bounded_detail_value(value: serde_json::Value, depth: usize) -> serde_json::Value {
    if depth >= 8 {
        return serde_json::Value::String("detail nesting budget exhausted".into());
    }
    match value {
        serde_json::Value::String(value) => {
            serde_json::Value::String(value.chars().take(1_024).collect())
        }
        serde_json::Value::Array(values) => serde_json::Value::Array(
            values
                .into_iter()
                .take(64)
                .map(|value| bounded_detail_value(value, depth + 1))
                .collect(),
        ),
        serde_json::Value::Object(values) => serde_json::Value::Object(
            values
                .into_iter()
                .take(64)
                .map(|(key, value)| {
                    (
                        key.chars().take(128).collect(),
                        bounded_detail_value(value, depth + 1),
                    )
                })
                .collect(),
        ),
        value => value,
    }
}

pub(super) fn service_error_failure(
    stage: SchemaBootstrapFailureStage,
    error: &ServiceError,
) -> SchemaBootstrapFailure {
    let mut failure = simple_failure(
        stage,
        &error.response.code,
        &error.response.message,
        error.response.retryable,
    );
    failure.decision_id = error
        .response
        .decision_id
        .as_ref()
        .map(|value| value.chars().take(256).collect());
    failure
}

pub(super) fn receipt_v1(
    receipt: &SchemaBootstrapReceipt,
) -> Result<BootstrapDispatchReceiptV1, ServiceError> {
    Ok(BootstrapDispatchReceiptV1 {
        request_id: receipt.request_id.clone(),
        pin: BootstrapSchemaPinV1 {
            scope: scope_v1(&receipt.pin.scope),
            bundle_digest: receipt.pin.bundle_digest.clone(),
        },
        entity_type: receipt.entity_type.clone(),
        entity_id: receipt.entity_id.clone(),
        creation_sequence: receipt.creation_sequence,
        action_sequence: receipt.action_sequence,
        action_result: receipt
            .canonical_action_result_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(|error| ServiceError::new("backend_unavailable", error.to_string(), true))?,
        failure: receipt.failure.as_ref().map(|failure| BootstrapFailureV1 {
            stage: match failure.stage {
                SchemaBootstrapFailureStage::Validation => BootstrapFailureStageV1::Validation,
                SchemaBootstrapFailureStage::Authorization => {
                    BootstrapFailureStageV1::Authorization
                }
                SchemaBootstrapFailureStage::Creation => BootstrapFailureStageV1::Creation,
                SchemaBootstrapFailureStage::Action => BootstrapFailureStageV1::Action,
                SchemaBootstrapFailureStage::Persistence => BootstrapFailureStageV1::Persistence,
                SchemaBootstrapFailureStage::Conflict => BootstrapFailureStageV1::Conflict,
                SchemaBootstrapFailureStage::Budget => BootstrapFailureStageV1::Budget,
            },
            code: failure.code.clone(),
            message: failure.message.clone(),
            retryable: failure.retryable,
            decision_id: failure.decision_id.clone(),
            details: failure.details.clone(),
        }),
    })
}

pub(super) fn runtime_type(entity_type: &str) -> &str {
    entity_type.rsplit('.').next().unwrap_or(entity_type)
}

fn validation_error(
    kind: temper_wasm_sdk::data::ModuleDataErrorKind,
    code: &str,
    message: impl Into<String>,
) -> temper_wasm_sdk::data::ModuleDataError {
    temper_wasm_sdk::data::ModuleDataError::new(
        kind,
        code,
        message,
        temper_wasm_sdk::data::Retryability::Never,
    )
}
