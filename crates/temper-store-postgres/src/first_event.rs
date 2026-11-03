//! Creation-time migration: a durable cursor and at most 128 indexed probes per turn.
use crate::PostgresEventStore;

type CreationEventRow = (String, String, String, Option<String>, Option<String>);
use std::time::{Duration, Instant};
use temper_runtime::persistence::{PersistenceError, storage_error};

impl PostgresEventStore {
    /// Start the one-time catalog migration after schema migrations, without
    /// holding up the first serve. Interrupted work resumes from its saved key.
    pub fn start_first_event_backfill(&self) {
        let store = self.clone();
        tokio::spawn(async move {
            let started = Instant::now();
            loop {
                match store.backfill_first_event_batch().await {
                    Ok(Some((records, null_records))) => {
                        tracing::info!(
                            records,
                            null_records,
                            elapsed_ms = started.elapsed().as_millis() as u64,
                            "first_event_at backfill complete"
                        );
                        return;
                    }
                    Ok(None) => tokio::time::sleep(Duration::from_millis(10)).await,
                    Err(error) => {
                        tracing::error!(%error, "first_event_at backfill stopped; resumes on next start");
                        return;
                    }
                }
            }
        });
    }

    async fn backfill_first_event_batch(&self) -> Result<Option<(u64, u64)>, PersistenceError> {
        let mut tx = self.pool().begin().await.map_err(storage_error)?;
        crate::dbm::postgres_query!(
            &self.schema,
            "INSERT INTO {schema}first_event_backfill (id) VALUES (1) ON CONFLICT(id) DO NOTHING"
        )
        .execute(&mut *tx)
        .await
        .map_err(storage_error)?;
        let (tenant, entity_type, entity_id, mut records, mut null_records, complete): (String, String, String, i64, i64, bool) =
            crate::dbm::postgres_query_as!(&self.schema,
                "SELECT tenant, entity_type, entity_id, records, null_records, complete FROM {schema}first_event_backfill WHERE id=1 FOR UPDATE")
                .fetch_one(&mut *tx).await.map_err(storage_error)?;
        if complete {
            tx.commit().await.map_err(storage_error)?;
            return Ok(Some((records as u64, null_records as u64)));
        }
        let batch: Vec<CreationEventRow> =
            crate::dbm::postgres_query_as!(&self.schema,
                "SELECT c.tenant, c.entity_type, c.entity_id, c.first_event_at,
                        COALESCE(e.payload->>'timestamp', e.metadata->>'timestamp')
                 FROM {schema}entity_catalog c LEFT JOIN {schema}events e
                   ON e.tenant=c.tenant AND e.entity_type=c.entity_type AND e.entity_id=c.entity_id AND e.sequence_nr=1
                 WHERE (c.tenant, c.entity_type, c.entity_id) > ($1, $2, $3)
                 ORDER BY c.tenant, c.entity_type, c.entity_id LIMIT 128")
                .bind(&tenant).bind(&entity_type).bind(&entity_id)
                .fetch_all(&mut *tx).await.map_err(storage_error)?;
        for (tenant, entity_type, entity_id, existing, event_time) in &batch {
            let timestamp = existing
                .as_ref()
                .or(event_time.as_ref())
                .map(|timestamp| {
                    chrono::DateTime::parse_from_rfc3339(timestamp)
                        .map(|t| {
                            t.with_timezone(&chrono::Utc)
                                .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
                        })
                        .map_err(|error| PersistenceError::Serialization(error.to_string()))
                })
                .transpose()?;
            crate::dbm::postgres_query!(&self.schema,
                "UPDATE {schema}entity_catalog SET first_event_at=$4 WHERE tenant=$1 AND entity_type=$2 AND entity_id=$3 AND first_event_at IS NULL")
                .bind(tenant).bind(entity_type).bind(entity_id).bind(&timestamp)
                .execute(&mut *tx).await.map_err(storage_error)?;
            records += 1;
            null_records += i64::from(timestamp.is_none());
        }
        let done = batch.len() < 128;
        let (tenant, entity_type, entity_id) = batch
            .last()
            .map(|r| (r.0.clone(), r.1.clone(), r.2.clone()))
            .unwrap_or((tenant, entity_type, entity_id));
        crate::dbm::postgres_query!(&self.schema,
            "UPDATE {schema}first_event_backfill SET tenant=$1, entity_type=$2, entity_id=$3, records=$4, null_records=$5, complete=$6 WHERE id=1")
            .bind(tenant).bind(entity_type).bind(entity_id).bind(records).bind(null_records).bind(done)
            .execute(&mut *tx).await.map_err(storage_error)?;
        tx.commit().await.map_err(storage_error)?;
        Ok(done.then_some((records as u64, null_records as u64)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PostgresSchema, migration::run_migrations_in_schema};
    use sqlx::PgPool;
    use temper_runtime::persistence::EventStore;

    #[tokio::test]
    async fn first_event_backfill_resumes_and_preserves_unknown_creation_times() {
        let Some(url) = temper_test_resource::env_var("DATABASE_URL") else {
            return;
        };
        let pool = PgPool::connect(&url).await.unwrap();
        let schema =
            PostgresSchema::new(format!("first_{}", uuid::Uuid::new_v4().simple())).unwrap();
        run_migrations_in_schema(&pool, &schema).await.unwrap();
        let store = PostgresEventStore::with_schema(pool.clone(), schema.clone());
        for n in 0..259 {
            let id = format!("{n:04}");
            crate::dbm::postgres_query!(&schema,
                "INSERT INTO {schema}entity_catalog (tenant, entity_type, entity_id, status, fields, state, sequence_nr) VALUES ('default', 'Job', $1, 'Done', '{}', '{}', 2)")
                .bind(&id).execute(&pool).await.unwrap();
            let sequence = if n % 3 == 0 { 2_i64 } else { 1 };
            crate::dbm::postgres_query!(&schema,
                "INSERT INTO {schema}events (tenant, entity_type, entity_id, sequence_nr, event_type, payload, metadata) VALUES ('default', 'Job', $1, $2, 'Created', '{\"timestamp\":\"2026-01-01T01:00:00.123+01:00\"}', '{}')")
                .bind(&id).bind(sequence).execute(&pool).await.unwrap();
        }
        assert_eq!(store.backfill_first_event_batch().await.unwrap(), None);
        let records: i64 = crate::dbm::postgres_query_scalar!(
            &schema,
            "SELECT records FROM {schema}first_event_backfill WHERE id=1"
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(records, 128);
        // A fresh store resumes from that cursor.
        let store = PostgresEventStore::with_schema(pool.clone(), schema.clone());
        assert_eq!(store.backfill_first_event_batch().await.unwrap(), None);
        assert_eq!(
            store.backfill_first_event_batch().await.unwrap(),
            Some((259, 87))
        );
        let rows = store
            .load_entity_catalog_rows_pg("default", "Job", &["0001".into(), "0003".into()])
            .await
            .unwrap();
        assert_eq!(
            rows[0].state.as_ref().unwrap()["first_event_at"],
            "2026-01-01T00:00:00.123000000Z"
        );
        assert_eq!(
            rows[1].state.as_ref().unwrap()["first_event_at"],
            serde_json::Value::Null
        );
        crate::dbm::postgres_query!(&schema, "DELETE FROM {schema}events WHERE tenant='default' AND entity_type='Job' AND entity_id='0001' AND sequence_nr=1")
            .execute(&pool).await.unwrap();
        assert_eq!(
            store
                .first_event_at("default:Job:0001")
                .await
                .unwrap()
                .unwrap()
                .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
            "2026-01-01T00:00:00.123000000Z"
        );
        assert_eq!(
            store.first_event_at("default:Job:0003").await.unwrap(),
            None
        );
        let forged = serde_json::json!({"first_event_at":"2099-01-01T00:00:00Z"});
        for id in ["0001", "new"] {
            store
                .upsert_query_projection_with_state(
                    "default",
                    "Job",
                    id,
                    "Moved",
                    &serde_json::json!({}),
                    &forged,
                    3,
                )
                .await
                .unwrap();
        }
        let rows = store
            .load_entity_catalog_rows_pg("default", "Job", &["0001".into(), "new".into()])
            .await
            .unwrap();
        assert_eq!(
            rows[0].state.as_ref().unwrap()["first_event_at"],
            "2026-01-01T00:00:00.123000000Z"
        );
        assert_eq!(
            rows[1].state.as_ref().unwrap()["first_event_at"],
            "2099-01-01T00:00:00Z"
        );
        assert_eq!(
            store.backfill_first_event_batch().await.unwrap(),
            Some((259, 87))
        );
        sqlx::query(&format!(
            "DROP SCHEMA {} CASCADE",
            schema.prefix().trim_end_matches('.')
        ))
        .execute(&pool)
        .await
        .unwrap();
    }
}
