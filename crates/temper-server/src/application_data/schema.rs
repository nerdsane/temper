//! Bound-schema validation and shared governed write prechecks.

use temper_wasm_sdk::data::{
    ManifestEntityV1, ManifestPropertyV1, ManifestValueSourceV1, ModuleDataError,
    ModuleDataErrorKind,
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
        "Edm.Decimal" => value.as_str().is_some_and(decimal_lexical),
        "Edm.Guid" => value
            .as_str()
            .is_some_and(|text| guid_lexical(text) && uuid::Uuid::parse_str(text).is_ok()),
        "Edm.DateTimeOffset" => value
            .as_str()
            .is_some_and(|text| chrono::DateTime::parse_from_rfc3339(text).is_ok()),
        "Edm.String" => value.is_string(),
        "Edm.Binary" => value.as_str().is_some_and(binary_lexical),
        // CSDL references and named scalar aliases cross this ABI as canonical strings.
        _ => value.is_string(),
    }
}

fn decimal_lexical(value: &str) -> bool {
    if matches!(value, "NaN" | "INF" | "-INF") {
        return false;
    }
    let unsigned = value.strip_prefix(['+', '-']).unwrap_or(value);
    let exponent_index = unsigned.find(['e', 'E']);
    let (mantissa, exponent) = exponent_index.map_or((unsigned, None), |index| {
        (&unsigned[..index], Some(&unsigned[index + 1..]))
    });
    if exponent.is_some_and(|exponent| {
        let digits = exponent.strip_prefix(['+', '-']).unwrap_or(exponent);
        digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit())
    }) || unsigned.matches(['e', 'E']).count() > 1
    {
        return false;
    }
    let mut parts = mantissa.split('.');
    let whole = parts.next().unwrap_or_default();
    let fraction = parts.next();
    if parts.next().is_some()
        || whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
    {
        return false;
    }
    fraction
        .is_none_or(|digits| !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
}

fn guid_lexical(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

fn binary_lexical(value: &str) -> bool {
    let unpadded = value.trim_end_matches('=');
    let padding = value.len() - unpadded.len();
    if padding > 2
        || unpadded
            .bytes()
            .any(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')))
    {
        return false;
    }
    match unpadded.len() % 4 {
        0 => padding == 0,
        2 => {
            matches!(padding, 0 | 2)
                && matches!(unpadded.as_bytes().last(), Some(b'A' | b'Q' | b'g' | b'w'))
        }
        3 => {
            padding <= 1
                && matches!(
                    unpadded.as_bytes().last(),
                    Some(
                        b'A' | b'E'
                            | b'I'
                            | b'M'
                            | b'Q'
                            | b'U'
                            | b'Y'
                            | b'c'
                            | b'g'
                            | b'k'
                            | b'o'
                            | b's'
                            | b'w'
                            | b'0'
                            | b'4'
                            | b'8'
                    )
                )
        }
        _ => false,
    }
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
        validate_manifest_action_params(entity, action, params)
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
