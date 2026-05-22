use std::sync::OnceLock;

pub(super) fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name) // determinism-ok: read once at startup
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}

fn env_bool(name: &str, default: bool) -> bool {
    std::env::var(name) // determinism-ok: read once at startup
        .ok()
        .and_then(|v| match v.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Some(true),
            "0" | "false" | "no" | "off" => Some(false),
            _ => None,
        })
        .unwrap_or(default)
}

/// Default OData collection page size when `$top` is absent.
pub(in crate::odata) fn odata_default_page_size() -> usize {
    static DEFAULT_PAGE_SIZE: OnceLock<usize> = OnceLock::new();
    *DEFAULT_PAGE_SIZE.get_or_init(|| env_usize("TEMPER_ODATA_DEFAULT_PAGE_SIZE", 100))
}

/// Maximum OData collection page size.
pub(in crate::odata) fn odata_max_entities() -> usize {
    static MAX_ENTITIES: OnceLock<usize> = OnceLock::new();
    *MAX_ENTITIES.get_or_init(|| env_usize("TEMPER_ODATA_MAX_ENTITIES", 1000))
}

pub(super) fn entity_set_materialization_concurrency() -> usize {
    static CONCURRENCY: OnceLock<usize> = OnceLock::new();
    *CONCURRENCY
        .get_or_init(|| env_usize("TEMPER_ODATA_ENTITY_SET_MATERIALIZATION_CONCURRENCY", 16))
}

/// When true, list materialization reads each row from the durable
/// `entity_catalog` projection in a single batch query, avoiding the per-row
/// actor hydration cost. IDs that are absent from the catalog still fall
/// back to the actor path on a per-id basis.
pub(super) fn catalog_fast_read_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| env_bool("TEMPER_ODATA_CATALOG_FAST_READ", false))
}
