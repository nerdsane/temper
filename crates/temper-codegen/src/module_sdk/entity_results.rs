//! Entity-valued action result discovery and source emission.

use std::collections::BTreeMap;

use temper_spec::csdl::{CsdlDocument, EntityType};
use temper_wasm_sdk::data::{ManifestPropertyV1, ModuleDataGrant};

use super::names::rust_type_name;
use super::source_types::generated_rust_type;
use super::{ModuleSdkCodegenError, resolve_entity};

pub(super) fn resolve_csdl_entity<'a>(
    csdl: &'a CsdlDocument,
    canonical: &str,
) -> Option<&'a EntityType> {
    let (namespace, name) = canonical.rsplit_once('.')?;
    csdl.schemas
        .iter()
        .find(|schema| schema.namespace == namespace)
        .and_then(|schema| schema.entity_type(name))
}

pub(super) fn validate_entity_results(
    csdl: &CsdlDocument,
    grant: &ModuleDataGrant,
) -> Result<(), ModuleSdkCodegenError> {
    for entity_grant in &grant.entities {
        let (_, actions) = resolve_entity(csdl, entity_grant)?;
        for action in actions {
            if let Some(type_name) = action
                .return_type
                .as_ref()
                .map(|return_type| &return_type.type_name)
                && resolve_csdl_entity(csdl, type_name).is_some()
                && type_name != &entity_grant.entity_type
            {
                return Err(ModuleSdkCodegenError::UnsupportedEntityResult {
                    action: action.name.clone(),
                    entity_type: entity_grant.entity_type.clone(),
                    result_type: type_name.clone(),
                });
            }
        }
    }
    Ok(())
}

pub(super) fn generated_entity_names(
    csdl: &CsdlDocument,
    grant: &ModuleDataGrant,
) -> Result<BTreeMap<String, String>, ModuleSdkCodegenError> {
    let entity_types = grant
        .entities
        .iter()
        .map(|entity| entity.entity_type.clone())
        .collect::<std::collections::BTreeSet<_>>();
    let mut names = BTreeMap::new();
    let mut canonical_by_generated = BTreeMap::new();
    for entity_type in entity_types {
        let entity = resolve_csdl_entity(csdl, &entity_type)
            .ok_or_else(|| ModuleSdkCodegenError::MissingEntity(entity_type.clone()))?;
        let generated = rust_type_name(&entity.name);
        if canonical_by_generated
            .insert(generated.clone(), entity_type.clone())
            .is_some()
        {
            return Err(ModuleSdkCodegenError::IdentifierCollision(generated));
        }
        names.insert(entity_type, generated);
    }
    Ok(names)
}

pub(super) fn emit_entity_value_types(
    source: &mut String,
    generated: &str,
    properties: &[ManifestPropertyV1],
) {
    source.push_str(&format!(
        "#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]\n#[serde(transparent)]\npub struct {generated}Id(pub String);\n\n"
    ));
    source.push_str(&format!(
        "#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]\npub struct {generated} {{\n"
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
}
