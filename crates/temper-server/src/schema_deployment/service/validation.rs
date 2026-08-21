use super::*;

impl GovernedSchemaDeploymentService<'_> {
    pub(super) async fn validate_migrated_target_state(
        &self,
        tenant: &TenantId,
        scope: &SchemaScope,
        bundle_digest: &str,
        entity_type: &str,
        fields: &serde_json::Value,
    ) -> Result<(), ServiceError> {
        let (csdl_entity, enum_members, references, target_states) = {
            let registry = self.state.registry.read().map_err(|_| {
                ServiceError::new("migration_failed", "registry lock poisoned", true)
            })?;
            let config = registry
                .get_scoped_config_at_digest(tenant, scope, bundle_digest)
                .ok_or_else(|| {
                    ServiceError::new("migration_rejected", "target bundle is not staged", false)
                })?;
            let csdl_entity = config
                .csdl
                .schemas
                .iter()
                .flat_map(|schema| &schema.entity_types)
                .find(|candidate| candidate.name == entity_type)
                .cloned()
                .ok_or_else(|| {
                    ServiceError::new(
                        "migration_rejected",
                        format!("target CSDL has no entity type '{entity_type}'"),
                        false,
                    )
                })?;
            let enum_members = config
                .csdl
                .schemas
                .iter()
                .flat_map(|schema| &schema.enum_types)
                .map(|value| {
                    (
                        value.name.clone(),
                        value
                            .members
                            .iter()
                            .map(|member| member.name.clone())
                            .collect::<std::collections::BTreeSet<_>>(),
                    )
                })
                .collect::<BTreeMap<_, _>>();
            let target_spec = config.entities.get(entity_type).ok_or_else(|| {
                ServiceError::new(
                    "migration_rejected",
                    format!("target bundle has no entity type '{entity_type}'"),
                    false,
                )
            })?;
            let references = target_spec
                .table()
                .state_var_metadata
                .iter()
                .filter_map(|(field, metadata)| {
                    metadata
                        .entity_type
                        .as_ref()
                        .map(|target| (field.clone(), target.clone()))
                })
                .collect::<Vec<_>>();
            let target_states = target_spec
                .table()
                .states
                .iter()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>();
            (csdl_entity, enum_members, references, target_states)
        };
        let object = fields.as_object().ok_or_else(|| {
            ServiceError::new(
                "migration_rejected",
                "target state must be an object",
                false,
            )
        })?;
        let aliases = alias_values(object)?;
        for property in &csdl_entity.properties {
            let value = aliases
                .get(&temper_spec::to_snake_case(&property.name))
                .copied();
            if value.is_none() && !property.nullable && property.default_value.is_none() {
                return Err(ServiceError::new(
                    "migration_rejected",
                    format!("required target property '{}' is missing", property.name),
                    false,
                ));
            }
            if let Some(value) = value
                && !property_accepts(property, value, &enum_members)
            {
                return Err(ServiceError::new(
                    "migration_rejected",
                    format!("target property '{}' has the wrong type", property.name),
                    false,
                ));
            }
        }
        for field in aliases.keys() {
            if field == "status" {
                continue;
            }
            if !csdl_entity
                .properties
                .iter()
                .any(|property| temper_spec::to_snake_case(&property.name) == *field)
            {
                return Err(ServiceError::new(
                    "migration_rejected",
                    format!("target state contains unknown property '{field}'"),
                    false,
                ));
            }
        }
        if let Some(status) = aliases.get("status") {
            let status = status.as_str().ok_or_else(|| {
                ServiceError::new(
                    "migration_rejected",
                    "target Status must be a string",
                    false,
                )
            })?;
            if !target_states.contains(status) {
                return Err(ServiceError::new(
                    "migration_rejected",
                    format!("target Status '{status}' is not declared by the target IOA"),
                    false,
                ));
            }
        }
        let (journal, _) = self.state.event_journal().ok_or_else(|| {
            ServiceError::new(
                "migration_failed",
                "target reference validation requires a durable event journal",
                true,
            )
        })?;
        for (field, target_entity_type) in references {
            let Some(target_id) = aliases
                .get(&temper_spec::to_snake_case(&field))
                .copied()
                .filter(|value| !value.is_null())
                .and_then(serde_json::Value::as_str)
            else {
                continue;
            };
            let persistence_id = format!(
                "{tenant}:{target_entity_type}:{}",
                temper_runtime::persistence::schema_deployment::scoped_journal_entity_id(
                    target_id,
                    &SchemaExecutionPin {
                        scope: scope.clone(),
                        bundle_digest: bundle_digest.to_string(),
                    },
                )
            );
            let exists = !journal
                .read_latest_events(&persistence_id, 1)
                .await
                .map_err(|error| ServiceError::new("migration_failed", error.to_string(), true))?
                .is_empty();
            if !exists {
                return Err(ServiceError::new(
                    "migration_rejected",
                    format!(
                        "typed reference '{field}' targets missing {target_entity_type}('{target_id}')"
                    ),
                    false,
                ));
            }
        }
        Ok(())
    }
}

fn alias_values(
    object: &serde_json::Map<String, serde_json::Value>,
) -> Result<BTreeMap<String, &serde_json::Value>, ServiceError> {
    let mut aliases = BTreeMap::new();
    for (field, value) in object {
        let canonical = temper_spec::to_snake_case(field);
        if aliases.insert(canonical.clone(), value).is_some() {
            return Err(ServiceError::new(
                "migration_rejected",
                format!("target state contains duplicate alias for '{canonical}'"),
                false,
            ));
        }
    }
    Ok(aliases)
}

pub(super) fn unambiguous_alias_value<'a>(
    fields: &'a serde_json::Value,
    field: &str,
) -> Result<Option<&'a serde_json::Value>, ServiceError> {
    let object = fields.as_object().ok_or_else(|| {
        ServiceError::new(
            "migration_rejected",
            "target state must be an object",
            false,
        )
    })?;
    Ok(alias_values(object)?
        .get(&temper_spec::to_snake_case(field))
        .copied())
}

fn property_accepts(
    property: &temper_spec::csdl::Property,
    value: &serde_json::Value,
    enum_members: &BTreeMap<String, std::collections::BTreeSet<String>>,
) -> bool {
    if value.is_null() {
        return property.nullable;
    }
    let short_type = property
        .type_name
        .rsplit('.')
        .next()
        .unwrap_or(&property.type_name);
    if let Some(members) = enum_members.get(short_type) {
        return value
            .as_str()
            .is_some_and(|member| members.contains(member));
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
        "Edm.Decimal" | "Edm.String" | "Edm.Binary" | "Edm.Guid" | "Edm.DateTimeOffset" => {
            value.is_string()
        }
        _ => value.is_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_aliases_are_unambiguous_and_status_is_case_insensitive() {
        let duplicate = serde_json::json!({"Status": "Draft", "status": "Bogus"});
        assert!(unambiguous_alias_value(&duplicate, "Status").is_err());

        let lowercase = serde_json::json!({"status": "Bogus"});
        assert_eq!(
            unambiguous_alias_value(&lowercase, "Status")
                .expect("single alias should resolve")
                .and_then(serde_json::Value::as_str),
            Some("Bogus")
        );
    }
}
