//! Schema-driven validation for bound action request bodies.

use std::collections::{BTreeMap, BTreeSet};

use temper_runtime::tenant::TenantId;
use temper_spec::csdl::{Action, CsdlDocument, Parameter};

use crate::state::ServerState;

/// Stable client-facing action input validation failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ActionInputViolation {
    pub(super) code: &'static str,
    pub(super) message: String,
}

/// Validate a bound action body against the invocation tenant's active CSDL.
pub(super) fn validate_bound_action_input(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    action_name: &str,
    body: &serde_json::Value,
) -> Result<(), ActionInputViolation> {
    let tenant_csdl = state
        .registry
        .read()
        .unwrap() // ci-ok: poisoned registry is a fail-fast invariant breach
        .get_tenant(tenant)
        .map(|config| config.csdl.clone());
    let csdl = tenant_csdl.unwrap_or_else(|| state.csdl.clone());
    let action_name = action_name.rsplit('.').next().unwrap_or(action_name);
    let Some(action) = find_bound_action(&csdl, entity_type, action_name) else {
        return Ok(());
    };
    validate_action_body(entity_type, action, body)
}

fn find_bound_action<'a>(
    csdl: &'a CsdlDocument,
    entity_type: &str,
    action_name: &str,
) -> Option<&'a Action> {
    csdl.schemas
        .iter()
        .flat_map(|schema| &schema.actions)
        .find(|action| {
            action.name == action_name
                && action.is_bound
                && action
                    .parameters
                    .first()
                    .is_some_and(|binding| type_tail(&binding.type_name) == entity_type)
        })
}

fn validate_action_body(
    entity_type: &str,
    action: &Action,
    body: &serde_json::Value,
) -> Result<(), ActionInputViolation> {
    let Some(object) = body.as_object() else {
        return Err(type_mismatch(entity_type, action, "<body>", "JSON object"));
    };

    let mut normalized_body = BTreeMap::new();
    for (name, value) in object {
        let normalized = temper_spec::naming::to_snake_case(name);
        if normalized_body.insert(normalized.clone(), value).is_some() {
            return Err(type_mismatch(
                entity_type,
                action,
                name,
                "one unambiguous parameter name",
            ));
        }
    }

    let mut declared = BTreeSet::new();
    for parameter in action.parameters.iter().skip(1) {
        let normalized = temper_spec::naming::to_snake_case(&parameter.name);
        declared.insert(normalized.clone());
        let value = normalized_body.get(&normalized).copied();
        if value.is_none_or(serde_json::Value::is_null) {
            if !parameter.nullable {
                return Err(ActionInputViolation {
                    code: "MissingActionParameter",
                    message: format!(
                        "action '{}.{}' requires non-null parameter '{}'",
                        entity_type, action.name, parameter.name
                    ),
                });
            }
            continue;
        }
        if !value.is_some_and(|value| value_matches_type(value, parameter)) {
            return Err(type_mismatch(
                entity_type,
                action,
                &parameter.name,
                &parameter.type_name,
            ));
        }
    }

    if let Some(extra) = normalized_body
        .keys()
        .find(|name| !declared.contains(*name))
    {
        return Err(type_mismatch(
            entity_type,
            action,
            extra,
            "a declared action parameter",
        ));
    }
    Ok(())
}

fn type_mismatch(
    entity_type: &str,
    action: &Action,
    parameter: &str,
    expected: &str,
) -> ActionInputViolation {
    ActionInputViolation {
        code: "ActionParameterTypeMismatch",
        message: format!(
            "action '{}.{}' parameter '{}' must match {}",
            entity_type, action.name, parameter, expected
        ),
    }
}

fn value_matches_type(value: &serde_json::Value, parameter: &Parameter) -> bool {
    value_matches_type_name(value, &parameter.type_name)
}

fn value_matches_type_name(value: &serde_json::Value, type_name: &str) -> bool {
    if let Some(element_type) = type_name
        .strip_prefix("Collection(")
        .and_then(|name| name.strip_suffix(')'))
    {
        return value.as_array().is_some_and(|values| {
            values
                .iter()
                .all(|value| value_matches_type_name(value, element_type))
        });
    }
    match type_name {
        "Edm.Boolean" => value.is_boolean(),
        "Edm.Byte" => integer_in_range(value, 0, u8::MAX as i128),
        "Edm.SByte" => integer_in_range(value, i8::MIN as i128, i8::MAX as i128),
        "Edm.Int16" => integer_in_range(value, i16::MIN as i128, i16::MAX as i128),
        "Edm.Int32" => integer_in_range(value, i32::MIN as i128, i32::MAX as i128),
        "Edm.Int64" => integer_in_range(value, i64::MIN as i128, i64::MAX as i128),
        "Edm.Decimal" | "Edm.Double" | "Edm.Single" => value.is_number(),
        "Edm.Binary" | "Edm.Date" | "Edm.DateTimeOffset" | "Edm.Duration" | "Edm.Guid"
        | "Edm.String" | "Edm.TimeOfDay" => value.is_string(),
        _ => value.is_string() || value.is_object(),
    }
}

fn integer_in_range(value: &serde_json::Value, minimum: i128, maximum: i128) -> bool {
    value
        .as_i64()
        .map(i128::from)
        .or_else(|| value.as_u64().map(i128::from))
        .is_some_and(|value| (minimum..=maximum).contains(&value))
}

fn type_tail(type_name: &str) -> &str {
    type_name.rsplit('.').next().unwrap_or(type_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use temper_spec::csdl::parse_csdl;

    fn action() -> (CsdlDocument, String) {
        let csdl = parse_csdl(
            r#"<?xml version="1.0"?><edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx"><edmx:DataServices><Schema Namespace="T" xmlns="http://docs.oasis-open.org/odata/ns/edm"><Action Name="Assign" IsBound="true"><Parameter Name="bindingParameter" Type="T.Task" Nullable="false"/><Parameter Name="AgentId" Type="Edm.String" Nullable="false"/><Parameter Name="Note" Type="Edm.String"/></Action></Schema></edmx:DataServices></edmx:Edmx>"#,
        )
        .expect("CSDL");
        (csdl, "Assign".to_string())
    }

    #[test]
    fn required_missing_and_null_share_missing_code() {
        let (csdl, name) = action();
        let action = find_bound_action(&csdl, "Task", &name).unwrap();
        for body in [serde_json::json!({}), serde_json::json!({"AgentId": null})] {
            let error = validate_action_body("Task", action, &body).unwrap_err();
            assert_eq!(error.code, "MissingActionParameter");
        }
    }

    #[test]
    fn nullable_absent_null_and_value_are_valid() {
        let (csdl, name) = action();
        let action = find_bound_action(&csdl, "Task", &name).unwrap();
        for body in [
            serde_json::json!({"AgentId": "a"}),
            serde_json::json!({"AgentId": "a", "Note": null}),
            serde_json::json!({"agent_id": "a", "note": "hello"}),
        ] {
            validate_action_body("Task", action, &body).unwrap();
        }
    }

    #[test]
    fn wrong_type_and_extra_parameter_use_type_mismatch_code() {
        let (csdl, name) = action();
        let action = find_bound_action(&csdl, "Task", &name).unwrap();
        for body in [
            serde_json::json!({"AgentId": 4}),
            serde_json::json!({"AgentId": "a", "Other": true}),
        ] {
            let error = validate_action_body("Task", action, &body).unwrap_err();
            assert_eq!(error.code, "ActionParameterTypeMismatch");
        }
    }

    #[test]
    fn collection_elements_and_integer_widths_are_validated() {
        assert!(value_matches_type_name(
            &serde_json::json!([1, 2]),
            "Collection(Edm.Int16)"
        ));
        assert!(!value_matches_type_name(
            &serde_json::json!([1, "two"]),
            "Collection(Edm.Int16)"
        ));
        assert!(value_matches_type_name(&serde_json::json!(255), "Edm.Byte"));
        assert!(!value_matches_type_name(
            &serde_json::json!(256),
            "Edm.Byte"
        ));
        assert!(!value_matches_type_name(
            &serde_json::json!(2_147_483_648_u64),
            "Edm.Int32"
        ));
    }
}
