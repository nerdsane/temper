use crate::storage::EntityCatalogRow;

pub(in crate::odata) fn catalog_select_projection_fields(
    query_options: &temper_odata::query::types::QueryOptions,
) -> Option<&[String]> {
    let select = query_options.select.as_deref()?;
    if select.is_empty()
        || query_options.filter.is_some()
        || query_options.orderby.is_some()
        || query_options.expand.is_some()
    {
        return None;
    }

    Some(select)
}

fn selected_catalog_property(
    entity_type: &str,
    row: &EntityCatalogRow,
    prop: &str,
) -> Option<serde_json::Value> {
    match prop {
        "entity_type" => return Some(serde_json::json!(entity_type)),
        "entity_id" => return Some(serde_json::json!(row.entity_id)),
        "status" => return Some(serde_json::json!(row.status)),
        "events" => return Some(serde_json::json!([])),
        "sequence_nr" => return Some(serde_json::json!(row.sequence_nr)),
        _ => {}
    }

    if let Some(state) = row.state.as_ref() {
        if let Some(value) = state.get(prop) {
            return Some(value.clone());
        }
        if let Some(value) = state.get("fields").and_then(|fields| fields.get(prop)) {
            return Some(value.clone());
        }
    }

    if let Some(value) = row.fields.get(prop) {
        return Some(value.clone());
    }

    match prop {
        "fields" => Some(row.fields.clone()),
        "item_count" => Some(serde_json::json!(0)),
        "counters" => Some(serde_json::json!({})),
        "booleans" => Some(serde_json::json!({})),
        "lists" => Some(serde_json::json!({})),
        "total_event_count" => Some(serde_json::json!(row.sequence_nr)),
        _ => None,
    }
}

pub(super) fn catalog_row_to_selected_entity_body(
    entity_type: &str,
    entity_set_name: &str,
    row: EntityCatalogRow,
    select: &[String],
) -> serde_json::Value {
    let mut selected = serde_json::Map::new();
    for prop in select {
        if let Some(value) = selected_catalog_property(entity_type, &row, prop) {
            selected.insert(prop.clone(), value);
        }
    }
    selected.insert(
        "@odata.id".to_string(),
        serde_json::json!(format!("{}('{}')", entity_set_name, row.entity_id)),
    );
    serde_json::Value::Object(selected)
}
