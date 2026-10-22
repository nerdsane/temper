//! Count-only totals over the durable query-plane catalog.
//!
//! `entity_catalog` holds one row per live entity: a delete removes the row
//! (and a projection whose status is `Deleted` is removed rather than
//! upserted), so a deleted entity is in no total. Totals carry no ids, fields
//! or values.

use std::collections::BTreeMap;
use std::path::Path;

use temper_runtime::persistence::{PersistenceError, storage_error};
use tracing::instrument;

use super::TursoEventStore;
use crate::driver::{Database, Rows, params};

/// One `GROUP BY` over the `(tenant, entity_type)` index; no row is loaded.
const ENTITY_COUNTS_BY_TYPE_SQL: &str = "SELECT entity_type, COUNT(*) \
     FROM entity_catalog \
     WHERE tenant = ?1 \
     GROUP BY entity_type \
     ORDER BY entity_type";

impl TursoEventStore {
    /// Return the tenant's projected entity counts grouped by entity type.
    #[instrument(skip_all, fields(otel.name = "turso.projected_entity_counts_by_type", tenant))]
    pub async fn projected_entity_counts_by_type(
        &self,
        tenant: &str,
    ) -> Result<BTreeMap<String, u64>, PersistenceError> {
        let conn = self.configured_connection().await?;
        let rows = conn
            .query(ENTITY_COUNTS_BY_TYPE_SQL, params![tenant])
            .await
            .map_err(storage_error)?;
        collect_counts(rows).await
    }

    /// Count the tenant's catalog rows per entity type in a local database
    /// file, for use while its server is stopped.
    ///
    /// Opens the file with the same engine `temper serve` uses but without
    /// [`TursoEventStore::new`]'s migrations, and sets `query_only` so the
    /// connection cannot write. A missing file is an error rather than a new
    /// empty database.
    #[instrument(skip_all, fields(otel.name = "turso.read_only_entity_counts", tenant))]
    pub async fn read_only_entity_counts(
        path: &Path,
        tenant: &str,
    ) -> Result<BTreeMap<String, u64>, PersistenceError> {
        if !path.try_exists().map_err(storage_error)? {
            return Err(PersistenceError::Storage(format!(
                "database file not found: {}",
                path.display()
            )));
        }
        let path = path.to_str().ok_or_else(|| {
            PersistenceError::Storage(format!("database path is not UTF-8: {}", path.display()))
        })?;
        let db = Database::local(path).await.map_err(storage_error)?;
        let conn = db.connect().map_err(storage_error)?;
        conn.query("PRAGMA query_only = 1", ())
            .await
            .map_err(storage_error)?;
        let rows = conn
            .query(ENTITY_COUNTS_BY_TYPE_SQL, params![tenant])
            .await
            .map_err(storage_error)?;
        collect_counts(rows).await
    }
}

async fn collect_counts(mut rows: Rows) -> Result<BTreeMap<String, u64>, PersistenceError> {
    let mut counts = BTreeMap::new();
    while let Some(row) = rows.next().await.map_err(storage_error)? {
        let entity_type: String = row.get(0).map_err(storage_error)?;
        let count: i64 = row.get(1).map_err(storage_error)?;
        counts.insert(entity_type, count.max(0) as u64);
    }
    Ok(counts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn counts_live_catalog_rows_per_type_for_one_tenant() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("counts.db");
        let store = TursoEventStore::new(&format!("file:{}", path.display()), None)
            .await
            .expect("store opens");
        let fields = serde_json::json!({ "Title": "secret title" });
        for id in ["o-1", "o-2", "o-3"] {
            store
                .upsert_query_projection("acme", "Order", id, "Open", &fields, 1)
                .await
                .expect("upsert order");
        }
        store
            .upsert_query_projection("acme", "Invoice", "i-1", "Draft", &fields, 1)
            .await
            .expect("upsert invoice");
        store
            .upsert_query_projection("other", "Order", "x-1", "Open", &fields, 1)
            .await
            .expect("upsert other tenant");
        store
            .remove_query_projection("acme", "Order", "o-3")
            .await
            .expect("delete removes the catalog row");

        let expected = BTreeMap::from([("Invoice".to_string(), 1), ("Order".to_string(), 2)]);
        assert_eq!(
            store
                .projected_entity_counts_by_type("acme")
                .await
                .expect("online counts"),
            expected
        );
        drop(store);

        assert_eq!(
            TursoEventStore::read_only_entity_counts(&path, "acme")
                .await
                .expect("offline counts"),
            expected
        );
        assert!(
            TursoEventStore::read_only_entity_counts(&path, "nobody")
                .await
                .expect("unknown tenant")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn read_only_counts_refuse_a_missing_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("absent.db");
        assert!(
            TursoEventStore::read_only_entity_counts(&path, "acme")
                .await
                .is_err()
        );
        assert!(!path.exists(), "a missing file must not be created");
    }
}
