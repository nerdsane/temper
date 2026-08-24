//! Emission of capability-scoped entity clients.

use std::collections::BTreeMap;

use temper_wasm_sdk::data::{
    EntityDataGrant, ManifestActionV1, ManifestPropertyV1, ModuleDataGrant,
};

use super::names::rust_type_name;
use super::source_types::{emit_query_types, generated_rust_type, generated_type_name};

pub(super) struct EntityClientSpec<'a> {
    pub(super) generated: &'a str,
    pub(super) canonical: &'a str,
    pub(super) properties: &'a [ManifestPropertyV1],
    pub(super) actions: &'a [ManifestActionV1],
    pub(super) grant: &'a ModuleDataGrant,
    pub(super) entity_grant: &'a EntityDataGrant,
    pub(super) generated_entity_names: &'a BTreeMap<String, String>,
}

pub(super) fn emit_entity_client(source: &mut String, spec: EntityClientSpec<'_>) {
    let EntityClientSpec {
        generated,
        canonical,
        properties,
        actions,
        grant,
        entity_grant,
        generated_entity_names,
    } = spec;
    source.push_str(&format!(
        "#[derive(Debug, Clone, serde::Serialize)]\npub struct {generated}Create {{\n"
    ));
    for property in properties {
        let rust_type = generated_rust_type(property);
        let rust_type = if property.nullable {
            format!("Option<{rust_type}>")
        } else {
            rust_type
        };
        source.push_str(&format!(
            "    #[serde(rename = \"{}\")]\n    pub {}: {},\n",
            property.canonical_name, property.generated_name, rust_type
        ));
    }
    source.push_str("}\n\n");
    source.push_str(&format!(
        "#[derive(Debug, Clone, Default, serde::Serialize)]\npub struct {generated}Patch {{\n"
    ));
    for property in properties {
        let rust_type = generated_rust_type(property);
        let rust_type = if property.nullable {
            format!("Option<Option<{rust_type}>>")
        } else {
            format!("Option<{rust_type}>")
        };
        source.push_str(&format!(
            "    #[serde(rename = \"{}\", skip_serializing_if = \"Option::is_none\")]\n    pub {}: {},\n",
            property.canonical_name, property.generated_name, rust_type
        ));
    }
    source.push_str("}\n\n");
    for action in actions {
        let operation_granted = if action.composite {
            grant
                .operations
                .contains(&temper_wasm_sdk::data::DataOperationKind::CompositeInvoke)
        } else {
            grant
                .operations
                .contains(&temper_wasm_sdk::data::DataOperationKind::ActionInvoke)
        };
        if !operation_granted {
            continue;
        }
        let action_type = format!(
            "{}{}Input",
            generated,
            rust_type_name(&action.canonical_name)
        );
        source.push_str(&format!(
            "#[derive(Debug, Clone, serde::Serialize)]\npub struct {action_type} {{\n"
        ));
        for parameter in &action.parameters {
            let rust_type = generated_rust_type(parameter);
            let rust_type = if parameter.nullable {
                format!("Option<{rust_type}>")
            } else {
                rust_type
            };
            source.push_str(&format!(
                "    #[serde(rename = \"{}\")]\n    pub {}: {},\n",
                parameter.canonical_name, parameter.generated_name, rust_type
            ));
        }
        source.push_str("}\n\n");
    }
    emit_query_types(source, generated, properties, entity_grant);
    source.push_str(&format!(
        "pub struct {generated}Client {{ data: DataClient }}\nimpl {generated}Client {{\n    pub const ENTITY_TYPE: &'static str = \"{canonical}\";\n    pub fn new() -> Self {{ Self {{ data: DataClient::default() }} }}\n"
    ));
    if grant
        .operations
        .contains(&temper_wasm_sdk::data::DataOperationKind::EntityGet)
    {
        source.push_str(&format!("    pub fn get(&mut self, id: impl Into<String>) -> Result<TypedEntity<{generated}>, ModuleDataError> {{ decode_entity(self.data.call(DataOperationV1::EntityGet {{ entity_type: Self::ENTITY_TYPE.into(), entity_id: id.into(), at_least_sequence: None }})?) }}\n"));
    }
    if grant
        .operations
        .contains(&temper_wasm_sdk::data::DataOperationKind::EntityQuery)
    {
        source.push_str(&format!("    pub fn query(&mut self, filter: Option<{generated}Filter>, order_by: Vec<{generated}Order>, page: PageV1) -> Result<TypedPage<{generated}>, ModuleDataError> {{ decode_page(self.data.call(DataOperationV1::EntityQuery {{ entity_type: Self::ENTITY_TYPE.into(), filter: filter.map(|value| value.0), order_by: order_by.into_iter().map(|value| value.0).collect(), page }})?) }}\n"));
    }
    if grant
        .operations
        .contains(&temper_wasm_sdk::data::DataOperationKind::EntityCreate)
    {
        source.push_str(&format!("    pub fn create(&mut self, value: {generated}Create) -> Result<TypedWrite<{generated}>, ModuleDataError> {{ decode_write(self.data.call(DataOperationV1::EntityCreate {{ entity_type: Self::ENTITY_TYPE.into(), value: serde_json::to_value(value).map_err(invalid_generated_value)?.as_object().cloned().unwrap_or_default() }})?) }}\n"));
    }
    if grant
        .operations
        .contains(&temper_wasm_sdk::data::DataOperationKind::EntityPatch)
    {
        source.push_str(&format!("    pub fn patch(&mut self, id: impl Into<String>, expected_sequence: Option<u64>, value: {generated}Patch) -> Result<TypedWrite<{generated}>, ModuleDataError> {{ decode_write(self.data.call(DataOperationV1::EntityPatch {{ entity_type: Self::ENTITY_TYPE.into(), entity_id: id.into(), expected_sequence, value: serde_json::to_value(value).map_err(invalid_generated_value)?.as_object().cloned().unwrap_or_default() }})?) }}\n"));
    }
    for action in actions {
        let operation_granted = if action.composite {
            grant
                .operations
                .contains(&temper_wasm_sdk::data::DataOperationKind::CompositeInvoke)
        } else {
            grant
                .operations
                .contains(&temper_wasm_sdk::data::DataOperationKind::ActionInvoke)
        };
        if !operation_granted {
            continue;
        }
        let method = &action.generated_name;
        let action_type = format!(
            "{}{}Input",
            generated,
            rust_type_name(&action.canonical_name)
        );
        let operation = if action.composite {
            "CompositeInvoke"
        } else {
            "ActionInvoke"
        };
        let result_type = action.result_type.as_deref().map_or_else(
            || "serde_json::Value".to_string(),
            |type_name| {
                generated_entity_names
                    .get(type_name)
                    .cloned()
                    .unwrap_or_else(|| generated_type_name(type_name, &action.result_enum_members))
            },
        );
        source.push_str(&format!("    pub fn {method}(&mut self, id: impl Into<String>, expected_sequence: Option<u64>, params: {action_type}) -> Result<TypedAction<{result_type}>, ModuleDataError> {{ decode_action(self.data.call(DataOperationV1::{operation} {{ entity_type: Self::ENTITY_TYPE.into(), entity_id: id.into(), action: \"{}\".into(), expected_sequence, params: serde_json::to_value(params).map_err(invalid_generated_value)?.as_object().cloned().unwrap_or_default() }})?) }}\n", action.canonical_name));
    }
    if grant
        .operations
        .contains(&temper_wasm_sdk::data::DataOperationKind::FileRead)
        && (entity_grant
            .file_operations
            .contains(&temper_wasm_sdk::data::FileOperationKind::ContentRead)
            || entity_grant
                .file_operations
                .contains(&temper_wasm_sdk::data::FileOperationKind::VersionRead))
    {
        source.push_str("    pub fn open_file_read(&mut self, file_id: impl Into<String>, version_id: Option<String>) -> Result<OpenedFileRead, ModuleDataError> { decode_file_read(self.data.call(DataOperationV1::FileReadOpen { file_id: file_id.into(), version_id })?) }\n");
    }
    if grant
        .operations
        .contains(&temper_wasm_sdk::data::DataOperationKind::FileWrite)
        && entity_grant
            .file_operations
            .contains(&temper_wasm_sdk::data::FileOperationKind::ContentWrite)
    {
        source.push_str("    pub fn open_file_write(&mut self, file_id: impl Into<String>, expected_sequence: Option<u64>, content_length: Option<u64>, content_hash: Option<String>) -> Result<FileWriter, ModuleDataError> { decode_file_write(self.data.call(DataOperationV1::FileWriteOpen { file_id: file_id.into(), expected_sequence, content_length, content_hash })?) }\n");
        source.push_str("    pub fn commit_file_write(&mut self, stream_handle: u32) -> Result<DataResultV1, ModuleDataError> { self.data.call(DataOperationV1::FileWriteCommit { stream_handle }) }\n");
        source.push_str("    pub fn abort_file_stream(&mut self, stream_handle: u32) -> Result<DataResultV1, ModuleDataError> { self.data.call(DataOperationV1::FileStreamAbort { stream_handle }) }\n");
    }
    source.push_str("}\n\n");
}
