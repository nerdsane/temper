use temper_runtime::persistence::PersistenceError;
use temper_runtime::tenant::parse_persistence_id_parts;

pub(super) fn canonical_persistence_id(persistence_id: &str) -> Result<String, PersistenceError> {
    let (tenant, entity_type, entity_id) =
        parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
    Ok(format!("{tenant}:{entity_type}:{entity_id}"))
}

pub(super) fn canonical_test_persistence_id(persistence_id: &str) -> String {
    canonical_persistence_id(persistence_id)
        .unwrap_or_else(|error| panic!("invalid test persistence id '{persistence_id}': {error}"))
}
