//! Schema-driven validation for bound action request bodies.

use temper_runtime::persistence::schema_deployment::SchemaExecutionPin;
use temper_runtime::tenant::TenantId;
use temper_spec::csdl::{Action, CsdlDocument, Parameter};

use crate::action_input_contract::{
    ActionInputShapeError, named_type_shape_from_csdl, validate_action_input_shape,
    value_matches_schema_type,
};
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
    schema_pin: Option<&SchemaExecutionPin>,
    entity_type: &str,
    action_name: &str,
    body: &serde_json::Value,
) -> Result<(), ActionInputViolation> {
    let registry = state.registry.read().unwrap(); // ci-ok: poisoned registry is a fail-fast invariant breach
    let csdl = match schema_pin {
        Some(pin) => registry
            .get_scoped_config_at_digest(tenant, &pin.scope, &pin.bundle_digest)
            .map(|config| config.csdl.clone())
            .expect("verified schema pin must resolve its immutable CSDL"), // ci-ok: prechecked pin
        None => registry
            .get_tenant(tenant)
            .map(|config| config.csdl.clone())
            .unwrap_or_else(|| state.csdl.clone()),
    };
    let action_name = action_name.rsplit('.').next().unwrap_or(action_name);
    let Some(action) = find_bound_action(&csdl, entity_type, action_name) else {
        return Err(ActionInputViolation {
            code: "UnknownBoundAction",
            message: format!(
                "action '{entity_type}.{action_name}' has no matching bound CSDL action"
            ),
        });
    };
    validate_action_body(&csdl, entity_type, action, body)
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
    csdl: &CsdlDocument,
    entity_type: &str,
    action: &Action,
    body: &serde_json::Value,
) -> Result<(), ActionInputViolation> {
    let Some(object) = body.as_object() else {
        return Err(type_mismatch(entity_type, action, "<body>", "JSON object"));
    };

    let values = validate_action_input_shape(
        object,
        action
            .parameters
            .iter()
            .skip(1)
            .map(|parameter| (parameter.name.as_str(), parameter.nullable)),
    )
    .map_err(|error| match error {
        ActionInputShapeError::Missing { parameter } => ActionInputViolation {
            code: "MissingActionParameter",
            message: format!(
                "action '{}.{}' requires non-null parameter '{}'",
                entity_type, action.name, parameter
            ),
        },
        ActionInputShapeError::Mismatch { parameter } => type_mismatch(
            entity_type,
            action,
            &parameter,
            "one declared, unambiguous action parameter",
        ),
    })?;

    for parameter in action.parameters.iter().skip(1) {
        if let Some(value) = values.get(parameter.name.as_str())
            && !value_matches_type(csdl, value, parameter)
        {
            return Err(type_mismatch(
                entity_type,
                action,
                &parameter.name,
                &parameter.type_name,
            ));
        }
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

fn value_matches_type(
    csdl: &CsdlDocument,
    value: &serde_json::Value,
    parameter: &Parameter,
) -> bool {
    value_matches_schema_type(
        value,
        &parameter.type_name,
        named_type_shape_from_csdl(csdl, &parameter.type_name),
    )
}

fn type_tail(type_name: &str) -> &str {
    type_name.rsplit('.').next().unwrap_or(type_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action_input_contract::NamedTypeShape;
    use temper_spec::csdl::parse_csdl;

    fn action() -> (CsdlDocument, String) {
        let csdl = parse_csdl(
            r#"<?xml version="1.0"?><edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx"><edmx:DataServices><Schema Namespace="T" xmlns="http://docs.oasis-open.org/odata/ns/edm"><EntityType Name="Task"></EntityType><EnumType Name="Status"><Member Name="Open"/><Member Name="Closed"/></EnumType><ComplexType Name="Payload"><Property Name="Value" Type="Edm.String"/></ComplexType><Action Name="Assign" IsBound="true"><Parameter Name="bindingParameter" Type="T.Task" Nullable="false"/><Parameter Name="AgentId" Type="Edm.String" Nullable="false"/><Parameter Name="Note" Type="Edm.String"/><Parameter Name="Status" Type="T.Status"/><Parameter Name="Payload" Type="T.Payload"/></Action></Schema></edmx:DataServices></edmx:Edmx>"#,
        )
        .expect("CSDL");
        (csdl, "Assign".to_string())
    }

    #[test]
    fn required_missing_and_null_share_missing_code() {
        let (csdl, name) = action();
        let action = find_bound_action(&csdl, "Task", &name).unwrap();
        for body in [serde_json::json!({}), serde_json::json!({"AgentId": null})] {
            let error = validate_action_body(&csdl, "Task", action, &body).unwrap_err();
            assert_eq!(error.code, "MissingActionParameter");
        }
    }

    #[test]
    fn nullable_absent_null_and_value_are_valid() {
        let (csdl, name) = action();
        let action = find_bound_action(&csdl, "Task", &name).unwrap();
        for body in [
            serde_json::json!({"AgentId": "a"}),
            serde_json::json!({"AgentId": ""}),
            serde_json::json!({"AgentId": "a", "Note": null}),
            serde_json::json!({"agent_id": "a", "note": "hello"}),
        ] {
            validate_action_body(&csdl, "Task", action, &body).unwrap();
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
            let error = validate_action_body(&csdl, "Task", action, &body).unwrap_err();
            assert_eq!(error.code, "ActionParameterTypeMismatch");
        }
    }

    #[test]
    fn named_enum_and_complex_shapes_are_resolved_from_csdl() {
        let (csdl, name) = action();
        let action = find_bound_action(&csdl, "Task", &name).unwrap();
        validate_action_body(
            &csdl,
            "Task",
            action,
            &serde_json::json!({
                "AgentId": "a",
                "Status": "Open",
                "Payload": {"Value": "ok"}
            }),
        )
        .unwrap();
        for body in [
            serde_json::json!({"AgentId": "a", "Status": "Unknown"}),
            serde_json::json!({"AgentId": "a", "Payload": "not-an-object"}),
        ] {
            let error = validate_action_body(&csdl, "Task", action, &body).unwrap_err();
            assert_eq!(error.code, "ActionParameterTypeMismatch");
        }
    }

    #[test]
    fn unmatched_bound_action_fails_closed() {
        let (csdl, _) = action();
        let xml = r#"<?xml version="1.0"?><edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx"><edmx:DataServices><Schema Namespace="T" xmlns="http://docs.oasis-open.org/odata/ns/edm"><Action Name="Assign" IsBound="true"><Parameter Name="bindingParameter" Type="T.Task" Nullable="false"/><Parameter Name="AgentId" Type="Edm.String" Nullable="false"/></Action></Schema></edmx:DataServices></edmx:Edmx>"#;
        let state = crate::state::ServerState::new(
            temper_runtime::ActorSystem::new("unmatched-action"),
            csdl,
            xml.to_string(),
        );
        let error = validate_bound_action_input(
            &state,
            &temper_runtime::tenant::TenantId::default(),
            None,
            "Order",
            "NotAnAction",
            &serde_json::json!({}),
        )
        .unwrap_err();
        assert_eq!(error.code, "UnknownBoundAction");
    }

    #[test]
    fn directory_create_does_not_skip_schema() {
        let xml = include_str!("../../../../os-apps/temper-fs/specs/model.csdl.xml");
        let csdl = parse_csdl(xml).expect("FS CSDL");
        let state = crate::state::ServerState::new(
            temper_runtime::ActorSystem::new("directory-create"),
            csdl,
            xml.to_string(),
        );
        let tenant = temper_runtime::tenant::TenantId::default();
        for (body, expected) in [
            (serde_json::json!({}), "MissingActionParameter"),
            (
                serde_json::json!({"name": 7, "path": "/", "workspace_id": "ws"}),
                "ActionParameterTypeMismatch",
            ),
            (
                serde_json::json!({
                    "name": "docs",
                    "path": "/docs",
                    "workspace_id": "ws",
                    "extra": true
                }),
                "ActionParameterTypeMismatch",
            ),
        ] {
            let error = validate_bound_action_input(
                &state,
                &tenant,
                None,
                "Directory",
                "Temper.FS.Create",
                &body,
            )
            .unwrap_err();
            assert_eq!(error.code, expected, "body={body}");
        }
    }

    #[test]
    fn collection_elements_and_integer_widths_are_validated() {
        assert!(value_matches_schema_type(
            &serde_json::json!([1, 2]),
            "Collection(Edm.Int16)",
            NamedTypeShape::Complex,
        ));
        assert!(!value_matches_schema_type(
            &serde_json::json!([1, "two"]),
            "Collection(Edm.Int16)",
            NamedTypeShape::Complex,
        ));
        assert!(value_matches_schema_type(
            &serde_json::json!(255),
            "Edm.Byte",
            NamedTypeShape::Complex,
        ));
        assert!(!value_matches_schema_type(
            &serde_json::json!(256),
            "Edm.Byte",
            NamedTypeShape::Complex,
        ));
        assert!(!value_matches_schema_type(
            &serde_json::json!(2_147_483_648_u64),
            "Edm.Int32",
            NamedTypeShape::Complex,
        ));
    }
}
