//! Bounded envelope and result helpers.

use temper_wasm_sdk::data::{
    BatchItemV1, CommitToken, DataOperationV1, DataResponseV1, DataResultV1, ModuleDataError,
    ModuleDataErrorKind, Retryability,
};

const MAX_CANONICAL_IDENTIFIER_BYTES: usize = 256;
const COMPACT_OUTCOME_BYTES: usize = 3_840;

pub(super) fn reserve_compact_response(
    operation: &DataOperationV1,
    response_budget: usize,
) -> Result<(), ModuleDataError> {
    let outcomes = match operation {
        DataOperationV1::Batch { items } => items.len(),
        _ => 1,
    };
    let required = outcomes
        .saturating_mul(COMPACT_OUTCOME_BYTES)
        .saturating_add(256);
    if required > response_budget {
        return Err(data_error(
            ModuleDataErrorKind::BudgetExceeded,
            "ResponseReservationExceeded",
            "compact operation acknowledgement exceeds the response budget",
        ));
    }
    Ok(())
}

pub(super) fn validate_operation_identifiers(
    operation: &DataOperationV1,
) -> Result<(), ModuleDataError> {
    fn identifier(value: &str) -> Result<(), ModuleDataError> {
        if value.is_empty() || value.len() > MAX_CANONICAL_IDENTIFIER_BYTES {
            return Err(data_error(
                ModuleDataErrorKind::InvalidRequest,
                "InvalidIdentifier",
                "canonical identifiers must contain between 1 and 256 UTF-8 bytes",
            ));
        }
        Ok(())
    }
    fn batch_item(item: &BatchItemV1) -> Result<(), ModuleDataError> {
        match item {
            BatchItemV1::EntityGet {
                entity_type,
                entity_id,
                ..
            }
            | BatchItemV1::EntityPatch {
                entity_type,
                entity_id,
                ..
            } => {
                identifier(entity_type)?;
                identifier(entity_id)
            }
            BatchItemV1::EntityCreate { entity_type, value } => {
                identifier(entity_type)?;
                super::extract_id(value).map(|_| ())
            }
            BatchItemV1::ActionInvoke {
                entity_type,
                entity_id,
                action,
                ..
            } => {
                identifier(entity_type)?;
                identifier(entity_id)?;
                identifier(action)
            }
        }
    }
    match operation {
        DataOperationV1::EntityGet {
            entity_type,
            entity_id,
            ..
        }
        | DataOperationV1::EntityPatch {
            entity_type,
            entity_id,
            ..
        } => {
            identifier(entity_type)?;
            identifier(entity_id)
        }
        DataOperationV1::EntityQuery {
            entity_type,
            order_by,
            ..
        } => {
            identifier(entity_type)?;
            for order in order_by {
                if let temper_wasm_sdk::data::OrderV1::Property { field, .. } = order {
                    identifier(field)?;
                }
            }
            Ok(())
        }
        DataOperationV1::EntityCreate { entity_type, value } => {
            identifier(entity_type)?;
            super::extract_id(value).and_then(|id| identifier(&id))
        }
        DataOperationV1::ActionInvoke {
            entity_type,
            entity_id,
            action,
            ..
        }
        | DataOperationV1::CompositeInvoke {
            entity_type,
            entity_id,
            action,
            ..
        } => {
            identifier(entity_type)?;
            identifier(entity_id)?;
            identifier(action)
        }
        DataOperationV1::Batch { items } => items.iter().try_for_each(batch_item),
        DataOperationV1::FileReadOpen {
            file_id,
            version_id,
        } => {
            identifier(file_id)?;
            if let Some(version_id) = version_id {
                identifier(version_id)?;
            }
            Ok(())
        }
        DataOperationV1::FileWriteOpen { file_id, .. } => identifier(file_id),
        DataOperationV1::FileWriteCommit { .. } | DataOperationV1::FileStreamAbort { .. } => Ok(()),
    }
}

pub(super) fn validate_value_budget(
    value: &serde_json::Value,
    depth: usize,
    nodes: &mut usize,
    byte_budget: usize,
) -> Result<(), ModuleDataError> {
    *nodes = nodes.saturating_add(1);
    if depth > 16 || *nodes > byte_budget / 2 {
        return Err(data_error(
            ModuleDataErrorKind::BudgetExceeded,
            "PayloadStructureBudgetExceeded",
            "request object depth or element budget exceeded",
        ));
    }
    match value {
        serde_json::Value::String(value) if value.len() > byte_budget => Err(data_error(
            ModuleDataErrorKind::BudgetExceeded,
            "StringBudgetExceeded",
            "request string exceeds the byte budget",
        )),
        serde_json::Value::Array(values) => {
            for value in values {
                validate_value_budget(value, depth + 1, nodes, byte_budget)?;
            }
            Ok(())
        }
        serde_json::Value::Object(values) => {
            for (name, value) in values {
                if name.len() > byte_budget {
                    return Err(data_error(
                        ModuleDataErrorKind::BudgetExceeded,
                        "StringBudgetExceeded",
                        "request property name exceeds the byte budget",
                    ));
                }
                validate_value_budget(value, depth + 1, nodes, byte_budget)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

pub(super) fn compact_committed_results(response: &mut DataResponseV1) {
    fn compact_result(result: &mut DataResultV1) {
        match result {
            DataResultV1::Write {
                value,
                value_omitted,
                ..
            } => {
                *value = None;
                *value_omitted = true;
            }
            DataResultV1::Action {
                result,
                result_omitted,
                ..
            } => {
                *result = None;
                *result_omitted = true;
            }
            DataResultV1::FileCommitted { metadata, .. } => *metadata = None,
            DataResultV1::Batch { outcomes } => {
                for outcome in outcomes {
                    if let temper_wasm_sdk::data::DataOutcomeV1::Ok { result } = outcome {
                        compact_result(result);
                    }
                }
            }
            _ => {}
        }
    }
    if let temper_wasm_sdk::data::DataOutcomeV1::Ok { result } = &mut response.outcome {
        compact_result(result);
    }
}

pub(super) fn short_type(entity_type: &str) -> &str {
    entity_type.rsplit('.').next().unwrap_or(entity_type)
}

pub(super) fn extract_id(
    value: &serde_json::Map<String, serde_json::Value>,
) -> Result<String, ModuleDataError> {
    value
        .get("Id")
        .or_else(|| value.get("id"))
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            data_error(
                ModuleDataErrorKind::InvalidRequest,
                "MissingEntityId",
                "entity create value must contain a string Id",
            )
        })
}

pub(super) fn commit(entity_type: &str, entity_id: &str, sequence: u64) -> CommitToken {
    CommitToken {
        entity_type: entity_type.into(),
        entity_id: entity_id.into(),
        sequence,
    }
}

pub(super) fn write_result(
    entity_type: &str,
    entity_id: &str,
    sequence: u64,
    value: serde_json::Value,
) -> DataResultV1 {
    DataResultV1::Write {
        commit: commit(entity_type, entity_id, sequence),
        value: value.as_object().cloned(),
        value_omitted: false,
    }
}

pub(super) fn internal_error(error: String) -> ModuleDataError {
    tracing::error!(%error, "application-data internal operation failed");
    let normalized = error.to_ascii_lowercase();
    let (kind, code, retryability) =
        if normalized.contains("sequenceconflict") || normalized.contains("concurrency") {
            (
                ModuleDataErrorKind::Conflict,
                "SequenceConflict",
                Retryability::AfterRefresh,
            )
        } else if normalized.contains("not found") || normalized.contains("notfound") {
            (
                ModuleDataErrorKind::NotFound,
                "EntityNotFound",
                Retryability::Never,
            )
        } else if normalized.contains("already exists") {
            (
                ModuleDataErrorKind::AlreadyExists,
                "EntityAlreadyExists",
                Retryability::Never,
            )
        } else if normalized.contains("guard") || normalized.contains("invalid transition") {
            (
                ModuleDataErrorKind::GuardRejected,
                "ActionRejected",
                Retryability::Never,
            )
        } else {
            (
                ModuleDataErrorKind::Internal,
                "DataServiceFailure",
                Retryability::Never,
            )
        };
    ModuleDataError::new(
        kind,
        code,
        "application-data operation failed",
        retryability,
    )
}

pub(super) fn data_error(kind: ModuleDataErrorKind, code: &str, message: &str) -> ModuleDataError {
    ModuleDataError::new(
        kind,
        code,
        message.chars().take(256).collect::<String>(),
        Retryability::Never,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compacting_committed_action_preserves_token_and_marks_omission() {
        let commit = commit("Temper.Example.Customer", "customer-1", 7);
        let mut response = DataResponseV1::ok(DataResultV1::Action {
            commit: commit.clone(),
            result: Some(serde_json::json!({"Id": "customer-1"})),
            result_omitted: false,
        });

        compact_committed_results(&mut response);

        let temper_wasm_sdk::data::DataOutcomeV1::Ok {
            result:
                DataResultV1::Action {
                    commit: compacted_commit,
                    result: None,
                    result_omitted: true,
                },
        } = response.outcome
        else {
            panic!("compacted action must remain a successful commit")
        };
        assert_eq!(compacted_commit, commit);
    }
}
