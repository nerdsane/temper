//! `temper entity-counts` subcommand.
//!
//! Prints one tenant's durable entity totals per type from a stopped server's
//! local database file, in the same shape as `GET /_admin/entity-counts`.
//! Reads only the query-plane catalog; prints no ids, fields or values.

use std::io::Write as _;
use std::path::Path;

use anyhow::Result;
use temper_store_turso::TursoEventStore;

/// Run the `temper entity-counts` subcommand.
///
/// Output goes through a locked handle so a closed pipe is an error, not a
/// panic.
pub async fn run(db: &Path, tenant: &str) -> Result<()> {
    let counts = counts_json(db, tenant).await?;
    writeln!(std::io::stdout().lock(), "{counts}")?;
    Ok(())
}

async fn counts_json(db: &Path, tenant: &str) -> Result<serde_json::Value> {
    let counts = TursoEventStore::read_only_entity_counts(db, tenant)
        .await
        .map_err(|e| anyhow::anyhow!("reading entity counts from {}: {e}", db.display()))?;
    Ok(serde_json::json!({ "tenant": tenant, "counts": counts }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn counts_a_fixture_database() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("app.db");
        let store = TursoEventStore::new(&format!("file:{}", path.display()), None)
            .await
            .expect("store opens");
        let fields = serde_json::json!({ "Note": "private" });
        for id in ["p-1", "p-2"] {
            store
                .upsert_query_projection("default", "Plan", id, "Active", &fields, 1)
                .await
                .expect("upsert plan");
        }
        store
            .upsert_query_projection("default", "Step", "s-1", "Todo", &fields, 1)
            .await
            .expect("upsert step");
        drop(store);

        assert_eq!(
            counts_json(&path, "default").await.expect("counts"),
            serde_json::json!({
                "tenant": "default",
                "counts": { "Plan": 2, "Step": 1 }
            })
        );
    }

    #[tokio::test]
    async fn refuses_a_missing_database_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(
            counts_json(&dir.path().join("absent.db"), "default")
                .await
                .is_err()
        );
    }
}
