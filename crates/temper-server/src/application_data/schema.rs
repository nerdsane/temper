//! Bound-schema validation and shared governed write prechecks.

use temper_wasm_sdk::data::{
    ManifestEntityV1, ManifestPropertyV1, ModuleDataError, ModuleDataErrorKind,
};

use crate::entity_actor::EntityState;

use super::{ApplicationDataInvocation, data_error, short_type};

fn property_accepts(property: &ManifestPropertyV1, value: &serde_json::Value) -> bool {
    if value.is_null() {
        return property.nullable;
    }
    if !property.enum_members.is_empty() {
        return value
            .as_str()
            .is_some_and(|member| property.enum_members.iter().any(|known| known == member));
    }
    match property.type_name.as_str() {
        "Edm.Boolean" => value.is_boolean(),
        "Edm.Byte" => value
            .as_u64()
            .is_some_and(|number| number <= u8::MAX.into()),
        "Edm.Int16" => value
            .as_i64()
            .is_some_and(|number| i16::try_from(number).is_ok()),
        "Edm.Int32" => value
            .as_i64()
            .is_some_and(|number| i32::try_from(number).is_ok()),
        "Edm.Int64" => value.as_i64().is_some(),
        "Edm.Single" | "Edm.Double" => value.as_f64().is_some_and(f64::is_finite),
        "Edm.Decimal" => value.as_str().is_some_and(canonical_decimal),
        "Edm.Guid" => value
            .as_str()
            .and_then(|text| uuid::Uuid::parse_str(text).ok())
            .is_some_and(|guid| {
                guid.hyphenated().to_string() == value.as_str().unwrap_or_default()
            }),
        "Edm.DateTimeOffset" => value
            .as_str()
            .is_some_and(|text| chrono::DateTime::parse_from_rfc3339(text).is_ok()),
        "Edm.String" | "Edm.Binary" => value.is_string(),
        // CSDL references and named scalar aliases cross this ABI as canonical strings.
        _ => value.is_string(),
    }
}

fn canonical_decimal(value: &str) -> bool {
    if value.is_empty() || value.starts_with('+') || value.ends_with('.') {
        return false;
    }
    let unsigned = value.strip_prefix('-').unwrap_or(value);
    if unsigned.is_empty() {
        return false;
    }
    let mut parts = unsigned.split('.');
    let whole = parts.next().unwrap_or_default();
    let fraction = parts.next();
    if parts.next().is_some()
        || whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || (whole.len() > 1 && whole.starts_with('0'))
    {
        return false;
    }
    fraction
        .is_none_or(|digits| !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
}

impl ApplicationDataInvocation {
    pub(super) fn canonical_entity_value(
        &self,
        entity_type: &str,
        state: &EntityState,
    ) -> serde_json::Map<String, serde_json::Value> {
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
            && entity
                .properties
                .iter()
                .any(|property| !property.nullable && !value.contains_key(&property.canonical_name))
        {
            return Err(data_error(
                ModuleDataErrorKind::SchemaMismatch,
                "MissingRequiredProperty",
                "required property is absent",
            ));
        }
        Ok(())
    }

    pub(super) fn validate_action_params(
        &self,
        entity_type: &str,
        action: &str,
        params: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<(), ModuleDataError> {
        let action = self
            .authority
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
            .ok_or_else(|| {
                data_error(
                    ModuleDataErrorKind::SchemaMismatch,
                    "UnknownAction",
                    "action is absent from the bound schema",
                )
            })?;
        for (name, value) in params {
            let parameter = action
                .parameters
                .iter()
                .find(|parameter| parameter.canonical_name == *name)
                .ok_or_else(|| {
                    data_error(
                        ModuleDataErrorKind::SchemaMismatch,
                        "UnknownActionParameter",
                        "action parameter is absent from the bound schema",
                    )
                })?;
            if !property_accepts(parameter, value) {
                return Err(data_error(
                    ModuleDataErrorKind::SchemaMismatch,
                    "ActionParameterTypeMismatch",
                    "action parameter does not match the bound schema",
                ));
            }
        }
        if action
            .parameters
            .iter()
            .any(|parameter| !parameter.nullable && !params.contains_key(&parameter.canonical_name))
        {
            return Err(data_error(
                ModuleDataErrorKind::SchemaMismatch,
                "MissingActionParameter",
                "required action parameter is absent",
            ));
        }
        Ok(())
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
        self.state
            .check_verification_gate(&self.authority.tenant, short_type(entity_type))
            .map_err(|_| {
                data_error(
                    ModuleDataErrorKind::VerificationFailed,
                    "VerificationGateRejected",
                    "entity specification is not verified",
                )
            })?;
        crate::odata::common::run_write_prechecks(
            &self.state,
            &self.authority.tenant,
            short_type(entity_type),
            entity_id,
            (action, operation),
            fields,
            None,
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

fn canonical_entity_value(
    schema: &ManifestEntityV1,
    state: &EntityState,
) -> serde_json::Map<String, serde_json::Value> {
    let fields = state
        .fields
        .as_object()
        .expect("committed entity fields must be a JSON object");
    let mut canonical = serde_json::Map::new();
    for property in &schema.properties {
        let normalized = temper_spec::to_snake_case(&property.canonical_name);
        let value = fields
            .get(&property.canonical_name)
            .or_else(|| {
                fields
                    .iter()
                    .find(|(name, _)| temper_spec::to_snake_case(name) == normalized)
                    .map(|(_, value)| value)
            })
            .cloned()
            .or_else(|| match normalized.as_str() {
                "id" => Some(serde_json::Value::String(state.entity_id.clone())),
                "status" => Some(serde_json::Value::String(state.status.clone())),
                _ => None,
            });
        if let Some(value) = value {
            canonical.insert(property.canonical_name.clone(), value);
        }
    }
    canonical
}
