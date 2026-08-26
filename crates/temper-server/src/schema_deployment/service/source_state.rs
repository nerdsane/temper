use std::collections::BTreeMap;

use super::ServiceError;

pub(super) fn migration_source_properties(
    canonical_csdl: &str,
    entity_types: &[String],
) -> Result<BTreeMap<String, Vec<String>>, ServiceError> {
    let csdl = temper_spec::parse_csdl(canonical_csdl)
        .map_err(|error| ServiceError::new("migration_rejected", error.to_string(), false))?;
    entity_types
        .iter()
        .map(|entity_type| {
            let properties = csdl
                .schemas
                .iter()
                .flat_map(|schema| &schema.entity_types)
                .find(|candidate| candidate.name == *entity_type)
                .ok_or_else(|| {
                    ServiceError::new(
                        "migration_rejected",
                        format!("source CSDL has no entity type '{entity_type}'"),
                        false,
                    )
                })?
                .properties
                .iter()
                .map(|property| property.name.clone())
                .collect();
            Ok((entity_type.clone(), properties))
        })
        .collect()
}

pub(super) fn canonicalize_migration_source_state(
    source_fields: &serde_json::Value,
    property_names: &[String],
) -> Result<serde_json::Value, ServiceError> {
    let source = source_fields.as_object().ok_or_else(|| {
        ServiceError::new(
            "migration_rejected",
            "source state fields must be an object",
            false,
        )
    })?;
    let mut aliases = BTreeMap::new();
    let mut canonical_owners = BTreeMap::new();
    for property_name in property_names {
        let canonical = temper_spec::to_snake_case(property_name);
        if let Some(existing) = canonical_owners.insert(canonical.clone(), property_name) {
            return Err(ServiceError::new(
                "migration_rejected",
                format!(
                    "source contract properties '{existing}' and '{property_name}' both map to '{canonical}'"
                ),
                false,
            ));
        }
        for alias in [property_name.as_str(), canonical.as_str()] {
            if let Some(existing) = aliases.insert(alias.to_string(), canonical.clone())
                && existing != canonical
            {
                return Err(ServiceError::new(
                    "migration_rejected",
                    format!("source contract contains ambiguous property alias '{alias}'"),
                    false,
                ));
            }
        }
    }
    for (alias, canonical) in [
        ("Id", "id"),
        ("id", "id"),
        ("Status", "status"),
        ("status", "status"),
    ] {
        if let Some(existing) = aliases.insert(alias.into(), canonical.into())
            && existing != canonical
        {
            return Err(ServiceError::new(
                "migration_rejected",
                format!("source contract contains ambiguous runtime alias '{alias}'"),
                false,
            ));
        }
    }

    let mut canonical_state = serde_json::Map::new();
    for (field, value) in source {
        if field == crate::entity_actor::SCHEMA_PIN_FIELD {
            continue;
        }
        let canonical = aliases.get(field).ok_or_else(|| {
            ServiceError::new(
                "migration_rejected",
                format!("source state contains unknown property '{field}'"),
                false,
            )
        })?;
        if let Some(existing) = canonical_state.get(canonical) {
            if existing != value {
                return Err(ServiceError::new(
                    "migration_rejected",
                    format!("source state aliases for '{canonical}' disagree"),
                    false,
                ));
            }
            continue;
        }
        canonical_state.insert(canonical.clone(), value.clone());
    }
    Ok(serde_json::Value::Object(canonical_state))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::canonicalize_migration_source_state;

    #[test]
    fn uses_contract_driven_snake_case_names() {
        let source = json!({
            "Id": "task-1",
            "id": "task-1",
            "Status": "Open",
            "status": "Open",
            "TaskId": "arc-1",
            "Title": "Typed boundary"
        });

        let canonical = canonicalize_migration_source_state(
            &source,
            &["Id".into(), "TaskId".into(), "Title".into()],
        )
        .expect("verified CSDL aliases should canonicalize");

        assert_eq!(
            canonical,
            json!({
                "id": "task-1",
                "status": "Open",
                "task_id": "arc-1",
                "title": "Typed boundary"
            })
        );
    }

    #[test]
    fn rejects_unknown_and_conflicting_aliases() {
        let properties = ["Id".into(), "Title".into()];
        let conflict = canonicalize_migration_source_state(
            &json!({"Title": "one", "title": "two"}),
            &properties,
        )
        .expect_err("conflicting aliases must fail");
        assert_eq!(conflict.code(), "migration_rejected");

        let unknown = canonicalize_migration_source_state(
            &json!({"Id": "task-1", "Undeclared": true}),
            &properties,
        )
        .expect_err("fields outside the verified CSDL contract must fail");
        assert_eq!(unknown.code(), "migration_rejected");
    }

    #[test]
    fn rejects_csdl_properties_with_the_same_snake_name() {
        let error = canonicalize_migration_source_state(
            &json!({"TaskId": "task-1", "task_id": "task-1"}),
            &["TaskId".into(), "task_id".into()],
        )
        .expect_err("distinct CSDL properties must not share one IOA name");

        assert_eq!(error.code(), "migration_rejected");
        assert!(error.message().contains("both map to 'task_id'"));
    }
}
