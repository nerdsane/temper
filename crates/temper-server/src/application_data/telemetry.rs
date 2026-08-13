use temper_wasm_sdk::data::{DataOperationV1, DataOutcomeV1, DataResponseV1, DataResultV1};

pub(super) fn record_operation_fields(operation: &DataOperationV1) {
    let span = tracing::Span::current();
    let (kind, entity_type, action, batch_count) = operation_fields(operation);
    span.record("operation_kind", kind);
    if let Some(entity_type) = entity_type {
        span.record("entity_type", entity_type);
    }
    if let Some(action) = action {
        span.record("action", action);
    }
    if let Some(batch_count) = batch_count {
        span.record("batch_count", batch_count);
    }
}

fn operation_fields(
    operation: &DataOperationV1,
) -> (&'static str, Option<&str>, Option<&str>, Option<usize>) {
    match operation {
        DataOperationV1::EntityGet { entity_type, .. } => {
            ("entity_get", Some(entity_type), None, None)
        }
        DataOperationV1::EntityQuery { entity_type, .. } => {
            ("entity_query", Some(entity_type), None, None)
        }
        DataOperationV1::EntityCreate { entity_type, .. } => {
            ("entity_create", Some(entity_type), None, None)
        }
        DataOperationV1::EntityPatch { entity_type, .. } => {
            ("entity_patch", Some(entity_type), None, None)
        }
        DataOperationV1::ActionInvoke {
            entity_type,
            action,
            ..
        } => ("action_invoke", Some(entity_type), Some(action), None),
        DataOperationV1::CompositeInvoke {
            entity_type,
            action,
            ..
        } => ("composite_invoke", Some(entity_type), Some(action), None),
        DataOperationV1::Batch { items } => ("batch", None, None, Some(items.len())),
        DataOperationV1::FileReadOpen { .. } => ("file_read", Some("File"), None, None),
        DataOperationV1::FileWriteOpen { .. } => ("file_write", Some("File"), None, None),
        DataOperationV1::FileWriteCommit { .. } => ("file_write_commit", Some("File"), None, None),
        DataOperationV1::FileStreamAbort { .. } => ("file_stream_abort", Some("File"), None, None),
    }
}

pub(super) fn result_kind(response: &DataResponseV1) -> &'static str {
    match &response.outcome {
        DataOutcomeV1::Error { .. } => "error",
        DataOutcomeV1::Ok { result } => match result {
            DataResultV1::Entity { .. } => "entity",
            DataResultV1::Page { .. } => "page",
            DataResultV1::Write { .. } => "write",
            DataResultV1::Action { .. } => "action",
            DataResultV1::Batch { .. } => "batch",
            DataResultV1::FileRead { .. } => "file_read",
            DataResultV1::FileWrite { .. } => "file_write",
            DataResultV1::FileCommitted { .. } => "file_committed",
            DataResultV1::Aborted => "aborted",
        },
    }
}

#[cfg(test)]
mod tests {
    use super::operation_fields;
    use temper_wasm_sdk::data::{BatchItemV1, DataOperationV1};

    #[test]
    fn derives_stable_operation_telemetry_fields() {
        let operation = DataOperationV1::Batch {
            items: vec![BatchItemV1::EntityGet {
                entity_type: "Temper.Task".into(),
                entity_id: "a".into(),
                at_least_sequence: None,
            }],
        };
        assert_eq!(operation_fields(&operation), ("batch", None, None, Some(1)));
        let action = DataOperationV1::ActionInvoke {
            entity_type: "Temper.Task".into(),
            entity_id: "a".into(),
            action: "Close".into(),
            expected_sequence: None,
            params: Default::default(),
        };
        assert_eq!(
            operation_fields(&action),
            ("action_invoke", Some("Temper.Task"), Some("Close"), None)
        );
    }
}
