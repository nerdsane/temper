use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use sha2::{Digest, Sha256};
use temper_spec::csdl::CsdlDocument;

pub(super) fn reject(
    app: &str,
    document: &CsdlDocument,
    seen: &mut BTreeMap<String, String>,
) -> Result<(), String> {
    for schema in &document.schemas {
        record_named_groups(
            app,
            "entity",
            &schema.namespace,
            &schema.entity_types,
            |value| &value.name,
            seen,
        )?;
        record_named_groups(
            app,
            "enum",
            &schema.namespace,
            &schema.enum_types,
            |value| &value.name,
            seen,
        )?;
        record_named_groups(
            app,
            "action",
            &schema.namespace,
            &schema.actions,
            |value| &value.name,
            seen,
        )?;
        record_named_groups(
            app,
            "function",
            &schema.namespace,
            &schema.functions,
            |value| &value.name,
            seen,
        )?;
        record_named_groups(
            app,
            "term",
            &schema.namespace,
            &schema.terms,
            |value| &value.name,
            seen,
        )?;
        for container in &schema.entity_containers {
            let scope = format!("{}.{}", schema.namespace, container.name);
            record_named_groups(
                app,
                "entity_set",
                &scope,
                &container.entity_sets,
                |value| &value.name,
                seen,
            )?;
            record_named_groups(
                app,
                "action_import",
                &scope,
                &container.action_imports,
                |value| &value.name,
                seen,
            )?;
            record_named_groups(
                app,
                "function_import",
                &scope,
                &container.function_imports,
                |value| &value.name,
                seen,
            )?;
        }
    }
    Ok(())
}

fn record_named_groups<T, F>(
    app: &str,
    kind: &str,
    scope: &str,
    values: &[T],
    name: F,
    seen: &mut BTreeMap<String, String>,
) -> Result<(), String>
where
    T: Serialize,
    F: Fn(&T) -> &str + Copy,
{
    let names = values
        .iter()
        .map(|value| name(value).to_string())
        .collect::<BTreeSet<_>>();
    for symbol_name in names {
        let group = values
            .iter()
            .filter(|value| name(value) == symbol_name)
            .collect::<Vec<_>>();
        let canonical = canonical_json(
            serde_json::to_value(&group)
                .map_err(|error| format!("failed to encode CSDL symbol: {error}"))?,
        );
        let bytes = serde_json::to_vec(&canonical)
            .map_err(|error| format!("failed to encode CSDL symbol: {error}"))?;
        let digest = format!("sha256:{:x}", Sha256::digest(bytes));
        let key = format!("{kind}:{scope}.{symbol_name}");
        if let Some(existing) = seen.insert(key.clone(), digest.clone())
            && existing != digest
        {
            return Err(format!(
                "schema conflict for '{key}' while loading app '{app}'"
            ));
        }
    }
    Ok(())
}

fn canonical_json(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.into_iter().map(canonical_json).collect())
        }
        serde_json::Value::Object(values) => {
            let mut entries = values.into_iter().collect::<Vec<_>>();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            serde_json::Value::Object(
                entries
                    .into_iter()
                    .map(|(name, value)| (name, canonical_json(value)))
                    .collect(),
            )
        }
        scalar => scalar,
    }
}
