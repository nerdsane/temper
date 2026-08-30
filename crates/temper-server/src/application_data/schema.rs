//! Bound-schema validation and shared governed write prechecks.

use temper_wasm_sdk::data::{
    ManifestEntityV1, ManifestPropertyV1, ManifestValueSourceV1, ModuleDataError,
    ModuleDataErrorKind,
};

use crate::action_input_contract::{
    ActionInputShapeError, NamedTypeShape, named_type_shape_from_csdl, validate_action_input_shape,
    value_matches_schema_type,
};
use crate::entity_actor::EntityState;

use super::{ApplicationDataInvocation, ModuleDataTarget, data_error, short_type};

#[cfg(feature = "test-helpers")]
/// Canonicalize committed entity state with the production module-data response path.
pub fn canonicalize_entity_for_test(
    schema: &ManifestEntityV1,
    state: &EntityState,
) -> Result<serde_json::Map<String, serde_json::Value>, ModuleDataError> {
    canonical_entity_value(schema, state)
}

fn property_accepts(property: &ManifestPropertyV1, value: &serde_json::Value) -> bool {
    if value.is_null() {
        return property.nullable;
    }
    let named_type = if property.enum_members.is_empty() {
        NamedTypeShape::EntityReference
    } else {
        NamedTypeShape::ManifestEnum(&property.enum_members)
    };
    value_matches_schema_type(value, &property.type_name, named_type)
}

fn action_parameter_accepts(
    csdl: &temper_spec::csdl::CsdlDocument,
    parameter: &ManifestPropertyV1,
    value: &serde_json::Value,
) -> bool {
    if value.is_null() {
        return parameter.nullable;
    }
    let named_type = named_type_shape_from_csdl(csdl, &parameter.type_name);
    value_matches_schema_type(value, &parameter.type_name, named_type)
}

impl ApplicationDataInvocation {
    pub(super) fn canonical_entity_value(
        &self,
        entity_type: &str,
        state: &EntityState,
    ) -> Result<serde_json::Map<String, serde_json::Value>, ModuleDataError> {
        let schema = self
            .authority
            .binding
            .entities
            .iter()
            .find(|entity| entity.entity_type == entity_type)
            .expect("granted entity type must exist in the bound schema");
        canonical_entity_value(schema, state)
    }

    pub(super) fn action_result_entity_type(
        &self,
        entity_type: &str,
        action: &str,
    ) -> Option<&str> {
        self.authority
            .binding
            .entities
            .iter()
            .find(|entity| entity.entity_type == entity_type)
            .and_then(|entity| {
                entity
                    .actions
                    .iter()
                    .find(|candidate| candidate.canonical_name == action)
            })
            .and_then(|action| action.result_type.as_deref())
            .filter(|result_type| *result_type == entity_type)
    }

    pub(super) fn validate_entity_object(
        &self,
        entity_type: &str,
        value: &serde_json::Map<String, serde_json::Value>,
        require_non_nullable: bool,
    ) -> Result<(), ModuleDataError> {
        let entity = self
            .authority
            .binding
            .entities
            .iter()
            .find(|entity| entity.entity_type == entity_type)
            .ok_or_else(|| {
                data_error(
                    ModuleDataErrorKind::SchemaMismatch,
                    "UnknownEntityType",
                    "entity type is absent from the bound schema",
                )
            })?;
        validate_manifest_entity_object(entity, value, require_non_nullable)
    }

    pub(super) fn validate_action_params(
        &self,
        entity_type: &str,
        action: &str,
        params: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<(), ModuleDataError> {
        let entity = self
            .authority
            .binding
            .entities
            .iter()
            .find(|entity| entity.entity_type == entity_type)
            .ok_or_else(|| {
                data_error(
                    ModuleDataErrorKind::SchemaMismatch,
                    "UnknownEntityType",
                    "entity type is absent from the bound schema",
                )
            })?;
        let registry = self.state.registry.read().unwrap(); // ci-ok: poisoned registry is fatal
        let csdl = match &self.authority.target {
            ModuleDataTarget::TenantGlobal => registry
                .get_tenant(&self.authority.tenant)
                .map(|config| &config.csdl)
                .unwrap_or(&self.state.csdl),
            ModuleDataTarget::Scoped(pin) => registry
                .get_scoped_config_at_digest(&self.authority.tenant, &pin.scope, &pin.bundle_digest)
                .map(|config| &config.csdl)
                .expect("verified module schema pin must resolve its immutable CSDL"),
        };
        validate_manifest_action_params(csdl, entity, action, params)
    }

    pub(super) async fn run_governed_write_prechecks(
        &self,
        entity_type: &str,
        entity_id: &str,
        action: &str,
        operation: &str,
        fields: &serde_json::Value,
    ) -> Result<(), ModuleDataError> {
        crate::odata::rate_limit::enforce_commons_write_rate_limit(
            &self.state,
            &self.authority.tenant,
            short_type(entity_type),
            crate::odata::rate_limit::owner_id_from_fields(fields),
            &self.authority.security,
        )
        .await
        .map_err(|response| {
            if response.status() == axum::http::StatusCode::TOO_MANY_REQUESTS {
                data_error(
                    ModuleDataErrorKind::BudgetExceeded,
                    "RateLimitExceeded",
                    "governed write rate limit rejected the operation",
                )
            } else {
                data_error(
                    ModuleDataErrorKind::Internal,
                    "RateLimitUnavailable",
                    "governed write rate limit is unavailable",
                )
            }
        })?;
        let schema_available = match &self.authority.target {
            ModuleDataTarget::TenantGlobal => self
                .state
                .check_verification_gate(&self.authority.tenant, short_type(entity_type))
                .is_ok(),
            ModuleDataTarget::Scoped(pin) => self
                .state
                .registry
                .read()
                .map(|registry| {
                    registry
                        .get_scoped_spec_at_digest(
                            &self.authority.tenant,
                            &pin.scope,
                            &pin.bundle_digest,
                            short_type(entity_type),
                        )
                        .is_some()
                })
                .unwrap_or(false),
        };
        if !schema_available {
            return Err(data_error(
                ModuleDataErrorKind::VerificationFailed,
                "VerificationGateRejected",
                "entity specification is not verified",
            ));
        }
        crate::odata::common::run_write_prechecks(
            &self.state,
            &self.authority.tenant,
            short_type(entity_type),
            entity_id,
            (action, operation),
            fields,
            self.authority.target.schema_pin(),
        )
        .await
        .map_err(|_| {
            data_error(
                ModuleDataErrorKind::RelationViolation,
                "WritePrecheckRejected",
                "governed write precheck rejected the operation",
            )
        })?;
        self.state
            .enforce_commons_verified_owner_for_write(
                &self.authority.tenant,
                short_type(entity_type),
                fields,
            )
            .await
            .map_err(|_| {
                data_error(
                    ModuleDataErrorKind::AuthorizationDenied,
                    "AccountVerificationRequired",
                    "commons account verification rejected the operation",
                )
            })?;
        self.state
            .enforce_commons_app_name_unique_for_write(
                &self.authority.tenant,
                short_type(entity_type),
                entity_id,
                fields,
            )
            .await
            .map_err(|_| {
                data_error(
                    ModuleDataErrorKind::AlreadyExists,
                    "UniqueConstraintViolation",
                    "governed uniqueness check rejected the operation",
                )
            })?;
        self.state
            .enforce_commons_storage_cap_for_write(
                &self.authority.tenant,
                short_type(entity_type),
                entity_id,
                action,
                fields,
            )
            .await
            .map_err(|_| {
                data_error(
                    ModuleDataErrorKind::BudgetExceeded,
                    "StorageCapExceeded",
                    "governed storage cap rejected the operation",
                )
            })?;
        Ok(())
    }
}

pub(crate) fn validate_manifest_entity_object(
    entity: &ManifestEntityV1,
    value: &serde_json::Map<String, serde_json::Value>,
    require_non_nullable: bool,
) -> Result<(), ModuleDataError> {
    for (name, field_value) in value {
        let property = entity
            .properties
            .iter()
            .find(|property| property.canonical_name == *name)
            .ok_or_else(|| {
                data_error(
                    ModuleDataErrorKind::SchemaMismatch,
                    "UnknownProperty",
                    "property is absent from the bound schema",
                )
            })?;
        if !property_accepts(property, field_value) {
            return Err(data_error(
                ModuleDataErrorKind::SchemaMismatch,
                "PropertyTypeMismatch",
                "property value does not match the bound schema",
            ));
        }
    }
    if require_non_nullable
        && entity.properties.iter().any(|property| {
            !property.nullable
                && property.source == ManifestValueSourceV1::StoredField
                && property.default_value.is_none()
                && !value.contains_key(&property.canonical_name)
        })
    {
        return Err(data_error(
            ModuleDataErrorKind::SchemaMismatch,
            "MissingRequiredProperty",
            "required property is absent",
        ));
    }
    Ok(())
}

pub(crate) fn validate_manifest_action_params(
    csdl: &temper_spec::csdl::CsdlDocument,
    entity: &ManifestEntityV1,
    action: &str,
    params: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), ModuleDataError> {
    let action = entity
        .actions
        .iter()
        .find(|candidate| candidate.canonical_name == action)
        .ok_or_else(|| {
            data_error(
                ModuleDataErrorKind::SchemaMismatch,
                "UnknownAction",
                "action is absent from the bound schema",
            )
        })?;
    let values = validate_action_input_shape(
        params,
        action
            .parameters
            .iter()
            .map(|parameter| (parameter.canonical_name.as_str(), parameter.nullable)),
    )
    .map_err(|error| match error {
        ActionInputShapeError::Missing { .. } => data_error(
            ModuleDataErrorKind::SchemaMismatch,
            "MissingActionParameter",
            "required action parameter is absent or null",
        ),
        ActionInputShapeError::Mismatch { .. } => data_error(
            ModuleDataErrorKind::SchemaMismatch,
            "ActionParameterTypeMismatch",
            "action parameter name is absent or ambiguous in the bound schema",
        ),
    })?;
    for parameter in &action.parameters {
        if let Some(value) = values.get(parameter.canonical_name.as_str())
            && !action_parameter_accepts(csdl, parameter, value)
        {
            return Err(data_error(
                ModuleDataErrorKind::SchemaMismatch,
                "ActionParameterTypeMismatch",
                "action parameter does not match the bound schema",
            ));
        }
    }
    Ok(())
}

pub(super) fn canonical_entity_value(
    schema: &ManifestEntityV1,
    state: &EntityState,
) -> Result<serde_json::Map<String, serde_json::Value>, ModuleDataError> {
    canonical_manifest_entity_value_from_parts(
        schema,
        &state.entity_id,
        &state.status,
        &state.fields,
    )
}

/// Render exact entity parts through one generated manifest entity.
pub(crate) fn canonical_manifest_entity_value_from_parts(
    schema: &ManifestEntityV1,
    entity_id: &str,
    status: &str,
    state_fields: &serde_json::Value,
) -> Result<serde_json::Map<String, serde_json::Value>, ModuleDataError> {
    let fields = state_fields
        .as_object()
        .expect("committed entity fields must be a JSON object");
    let mut canonical = serde_json::Map::new();
    for property in &schema.properties {
        let value = match property.source {
            ManifestValueSourceV1::StoredField => stored_property_value(fields, property)
                .cloned()
                .or_else(|| property.default_value.clone()),
            ManifestValueSourceV1::EntityId => {
                Some(serde_json::Value::String(entity_id.to_string()))
            }
            ManifestValueSourceV1::LifecycleStatus => {
                Some(serde_json::Value::String(status.to_string()))
            }
            ManifestValueSourceV1::Input => {
                return Err(data_error(
                    ModuleDataErrorKind::SchemaMismatch,
                    "InvalidEntityPropertySource",
                    "entity property has an input-only manifest source",
                ));
            }
        };
        let Some(value) = value else {
            if property.nullable {
                continue;
            }
            return Err(data_error(
                ModuleDataErrorKind::SchemaMismatch,
                "MissingRequiredProperty",
                "required property is absent and has no declared default",
            ));
        };
        if !property_accepts(property, &value) {
            return Err(data_error(
                ModuleDataErrorKind::SchemaMismatch,
                "PropertyTypeMismatch",
                "entity property value does not match the bound schema",
            ));
        }
        canonical.insert(property.canonical_name.clone(), value);
    }
    Ok(canonical)
}

fn stored_property_value<'a>(
    fields: &'a serde_json::Map<String, serde_json::Value>,
    property: &ManifestPropertyV1,
) -> Option<&'a serde_json::Value> {
    let normalized = temper_spec::to_snake_case(&property.canonical_name);
    fields.get(&property.canonical_name).or_else(|| {
        fields
            .iter()
            .find(|(name, _)| temper_spec::to_snake_case(name) == normalized)
            .map(|(_, value)| value)
    })
}

#[cfg(test)]
mod action_param_tests {
    use super::*;
    use temper_wasm_sdk::data::ManifestActionV1;

    fn csdl() -> temper_spec::csdl::CsdlDocument {
        temper_spec::parse_csdl(
            r#"<?xml version="1.0"?><edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx"><edmx:DataServices><Schema Namespace="Temper" xmlns="http://docs.oasis-open.org/odata/ns/edm"><EntityType Name="Task"></EntityType><EntityType Name="User"></EntityType><EnumType Name="Phase"><Member Name="Open"/><Member Name="Closed"/></EnumType><ComplexType Name="Payload"><Property Name="Value" Type="Edm.String"/></ComplexType></Schema></edmx:DataServices></edmx:Edmx>"#,
        )
        .expect("test CSDL")
    }

    fn entity() -> ManifestEntityV1 {
        ManifestEntityV1 {
            entity_type: "Temper.Task".into(),
            entity_set: "Tasks".into(),
            generated_name: "Task".into(),
            properties: Vec::new(),
            actions: vec![ManifestActionV1 {
                canonical_name: "Close".into(),
                generated_name: "close".into(),
                parameters: vec![
                    ManifestPropertyV1 {
                        canonical_name: "ReasonCode".into(),
                        generated_name: "reason_code".into(),
                        type_name: "Edm.String".into(),
                        nullable: false,
                        source: ManifestValueSourceV1::Input,
                        default_value: None,
                        enum_members: Vec::new(),
                    },
                    ManifestPropertyV1 {
                        canonical_name: "Payload".into(),
                        generated_name: "payload".into(),
                        type_name: "Temper.Payload".into(),
                        nullable: true,
                        source: ManifestValueSourceV1::Input,
                        default_value: None,
                        enum_members: Vec::new(),
                    },
                    ManifestPropertyV1 {
                        canonical_name: "Phase".into(),
                        generated_name: "phase".into(),
                        type_name: "Temper.Phase".into(),
                        nullable: true,
                        source: ManifestValueSourceV1::Input,
                        default_value: None,
                        enum_members: vec!["Open".into(), "Closed".into()],
                    },
                    ManifestPropertyV1 {
                        canonical_name: "Owner".into(),
                        generated_name: "owner".into(),
                        type_name: "Temper.User".into(),
                        nullable: true,
                        source: ManifestValueSourceV1::Input,
                        default_value: None,
                        enum_members: Vec::new(),
                    },
                ],
                result_type: None,
                result_enum_members: Vec::new(),
                composite: false,
            }],
        }
    }

    #[test]
    fn module_action_missing_and_null_required_values_share_the_stable_code() {
        for params in [
            serde_json::json!({}),
            serde_json::json!({"ReasonCode": null}),
        ] {
            let entity = entity();
            let error = validate_manifest_action_params(
                &csdl(),
                &entity,
                "Close",
                params.as_object().unwrap(),
            )
            .unwrap_err();
            assert_eq!(error.kind, ModuleDataErrorKind::SchemaMismatch);
            assert_eq!(error.code, "MissingActionParameter");
        }
    }

    #[test]
    fn module_action_aliases_are_accepted_and_extras_use_type_mismatch() {
        let entity = entity();
        validate_manifest_action_params(
            &csdl(),
            &entity,
            "Close",
            serde_json::json!({"reason_code": "done"})
                .as_object()
                .unwrap(),
        )
        .unwrap();
        let error = validate_manifest_action_params(
            &csdl(),
            &entity,
            "Close",
            serde_json::json!({"ReasonCode": "done", "Other": true})
                .as_object()
                .unwrap(),
        )
        .unwrap_err();
        assert_eq!(error.code, "ActionParameterTypeMismatch");
    }

    #[test]
    fn module_action_rejects_unknown_enum_and_wrong_reference_shape() {
        let entity = entity();
        for params in [
            serde_json::json!({"ReasonCode": "done", "Phase": "Unknown"}),
            serde_json::json!({"ReasonCode": "done", "Owner": {"Id": "user-1"}}),
            serde_json::json!({"ReasonCode": "done", "Payload": "not-an-object"}),
        ] {
            let result = validate_manifest_action_params(
                &csdl(),
                &entity,
                "Close",
                params.as_object().unwrap(),
            );
            let error = match result {
                Ok(()) => panic!("invalid params unexpectedly accepted: {params}"),
                Err(error) => error,
            };
            assert_eq!(error.code, "ActionParameterTypeMismatch");
        }
        validate_manifest_action_params(
            &csdl(),
            &entity,
            "Close",
            serde_json::json!({
                "ReasonCode": "done",
                "Phase": "Open",
                "Owner": "user-1",
                "Payload": {"Value": "ok"}
            })
            .as_object()
            .unwrap(),
        )
        .unwrap();
    }
}
