use std::collections::BTreeSet;

use sha2::{Digest, Sha256};
use temper_wasm_sdk::data::{EntityDataGrant, ManifestPropertyV1, ModuleSdkManifest};

use super::names::{rust_scalar_type, rust_type_name};

pub(super) fn emit_artifact_binding(source: &mut String, manifest: &ModuleSdkManifest) {
    for (name, digest) in [
        ("GRANT", &manifest.grant_digest),
        ("CLOSURE", &manifest.closure_digest),
        ("DEPENDENCY_LOCK", &manifest.dependency_lock_digest),
        ("SCHEMA", &manifest.schema_digest),
        ("USED_SYMBOLS", &manifest.used_symbols_digest),
    ] {
        source.push_str(&format!(
            "#[used]\npub static TEMPER_MODULE_{name}_DIGEST: &str = \"{digest}\";\n"
        ));
    }
}

pub(super) fn hex_sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(super) fn emit_named_property_type(
    source: &mut String,
    property: &ManifestPropertyV1,
    emitted: &mut BTreeSet<String>,
) {
    if property.type_name.starts_with("Edm.") {
        return;
    }
    let generated = rust_type_name(&property.type_name);
    if !emitted.insert(generated.clone()) {
        return;
    }
    if property.enum_members.is_empty() {
        source.push_str(&format!("#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]\n#[serde(transparent)]\npub struct {generated}Id(pub String);\n\n"));
    } else {
        source.push_str(&format!("#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]\npub enum {generated} {{\n"));
        for member in &property.enum_members {
            source.push_str(&format!(
                "    #[serde(rename = \"{member}\")]\n    {},\n",
                rust_type_name(member)
            ));
        }
        source.push_str("}\n\n");
    }
}

pub(super) fn generated_rust_type(property: &ManifestPropertyV1) -> String {
    if property.type_name.starts_with("Edm.") {
        rust_scalar_type(&property.type_name).into()
    } else if property.enum_members.is_empty() {
        format!("{}Id", rust_type_name(&property.type_name))
    } else {
        rust_type_name(&property.type_name)
    }
}

pub(super) fn generated_type_name(type_name: &str, enum_members: &[String]) -> String {
    if type_name.starts_with("Edm.") {
        rust_scalar_type(type_name).into()
    } else if !enum_members.is_empty() {
        rust_type_name(type_name)
    } else {
        format!("{}Id", rust_type_name(type_name))
    }
}

pub(super) fn emit_query_types(
    source: &mut String,
    generated: &str,
    properties: &[ManifestPropertyV1],
    grant: &EntityDataGrant,
) {
    source.push_str(&format!("pub struct {generated}Filter(FilterV1);\nimpl {generated}Filter {{\n    pub fn and(operands: Vec<Self>) -> Self {{ Self(FilterV1::And {{ operands: operands.into_iter().map(|value| value.0).collect() }}) }}\n    pub fn or(operands: Vec<Self>) -> Self {{ Self(FilterV1::Or {{ operands: operands.into_iter().map(|value| value.0).collect() }}) }}\n    pub fn not(operand: Self) -> Self {{ Self(FilterV1::Not {{ operand: Box::new(operand.0) }}) }}\n"));
    for property in properties
        .iter()
        .filter(|property| grant.query_filter_fields.contains(&property.canonical_name))
    {
        let field = &property.generated_name;
        let canonical = &property.canonical_name;
        let rust_type = generated_rust_type(property);
        let scalar = scalar_expression(property, "value");
        source.push_str(&format!("    pub fn {field}_eq(value: {rust_type}) -> Self {{ Self(FilterV1::Compare {{ field: \"{canonical}\".into(), operator: CompareOperatorV1::Eq, value: {scalar} }}) }}\n    pub fn {field}_ne(value: {rust_type}) -> Self {{ Self(FilterV1::Compare {{ field: \"{canonical}\".into(), operator: CompareOperatorV1::Ne, value: {scalar} }}) }}\n"));
        if property.type_name != "Edm.Boolean" {
            source.push_str(&format!("    pub fn {field}_lt(value: {rust_type}) -> Self {{ Self(FilterV1::Compare {{ field: \"{canonical}\".into(), operator: CompareOperatorV1::Lt, value: {scalar} }}) }}\n    pub fn {field}_le(value: {rust_type}) -> Self {{ Self(FilterV1::Compare {{ field: \"{canonical}\".into(), operator: CompareOperatorV1::Le, value: {scalar} }}) }}\n    pub fn {field}_gt(value: {rust_type}) -> Self {{ Self(FilterV1::Compare {{ field: \"{canonical}\".into(), operator: CompareOperatorV1::Gt, value: {scalar} }}) }}\n    pub fn {field}_ge(value: {rust_type}) -> Self {{ Self(FilterV1::Compare {{ field: \"{canonical}\".into(), operator: CompareOperatorV1::Ge, value: {scalar} }}) }}\n"));
        }
        if property.nullable {
            source.push_str(&format!("    pub fn {field}_is_null(is_null: bool) -> Self {{ Self(FilterV1::IsNull {{ field: \"{canonical}\".into(), is_null }}) }}\n"));
        }
    }
    source.push_str("}\n\n");
    source.push_str(&format!(
        "pub struct {generated}Order(OrderV1);\nimpl {generated}Order {{\n"
    ));
    for property in properties
        .iter()
        .filter(|property| grant.query_order_fields.contains(&property.canonical_name))
    {
        source.push_str(&format!("    pub fn {}(direction: OrderDirectionV1) -> Self {{ Self(OrderV1::property(\"{}\", direction)) }}\n", property.generated_name, property.canonical_name));
    }
    if grant.query_order_by_sequence {
        source.push_str("    pub fn commit_sequence(direction: OrderDirectionV1) -> Self { Self(OrderV1::EntityCommitSequence { direction }) }\n");
    }
    source.push_str("}\n\n");
}

fn scalar_expression(property: &ManifestPropertyV1, value: &str) -> String {
    match property.type_name.as_str() {
        "Edm.Boolean" => format!("ScalarV1::Boolean({value})"),
        "Edm.Byte" | "Edm.Int16" | "Edm.Int32" | "Edm.Int64" => format!("ScalarV1::Int64({value})"),
        "Edm.Single" | "Edm.Double" => format!("ScalarV1::Double({value})"),
        "Edm.Guid" => format!("ScalarV1::Guid({value})"),
        "Edm.DateTimeOffset" => format!("ScalarV1::DateTimeOffset({value})"),
        "Edm.Decimal" => format!("ScalarV1::Decimal({value})"),
        "Edm.String" => format!("ScalarV1::String({value})"),
        _ if property.enum_members.is_empty() => format!("ScalarV1::String({value}.0)"),
        _ => format!(
            "ScalarV1::Enum(EnumValueV1 {{ type_name: \"{}\".into(), member: serde_json::to_value({value}).expect(\"generated enum serializes\").as_str().expect(\"generated enum is a string\").into() }})",
            property.type_name
        ),
    }
}
