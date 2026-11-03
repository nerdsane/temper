//! Shared rows and schema helpers for real Postgres tests.

use super::*;

/// Assert the record's first move left a row in every table it writes
/// besides the journal's own, so a later "nothing changed" compares real rows.
/// On main the append wrote no catalog, field-index or timer rows.
pub(super) fn assert_first_move_rows(snapshot: &BTreeMap<&'static str, Vec<String>>) {
    for table in [
        "entity_key_index",
        "entity_vector_index",
        "scheduled_actions",
        "entity_catalog",
        "entity_field_index",
        "state_timers",
    ] {
        assert!(
            !snapshot[table].is_empty(),
            "the first move wrote no {table} row"
        );
    }
}

/// Every table a move writes, in the order a snapshot reads them.
pub(super) const TABLES: [&str; 9] = [
    "events",
    "event_segments",
    "entity_key_index",
    "entity_vector_index",
    "scheduled_actions",
    "entity_catalog",
    "entity_field_index",
    "entity_projection_delta",
    "state_timers",
];

pub(super) fn envelope(event_type: &str, payload: serde_json::Value) -> PersistenceEnvelope {
    PersistenceEnvelope {
        sequence_nr: 0,
        event_type: event_type.to_string(),
        payload,
        metadata: EventMetadata {
            event_id: uuid::Uuid::new_v4(),
            causation_id: uuid::Uuid::new_v4(),
            correlation_id: uuid::Uuid::new_v4(),
            timestamp: chrono::Utc::now(),
            actor_id: "append-commit-test".to_string(),
        },
    }
}

pub(super) fn tombstone() -> PersistenceEnvelope {
    envelope(
        "Deleted",
        serde_json::json!({
            "action": "Deleted", "from_status": "Open", "to_status": "Deleted",
            "params": {}, "kind": "tombstone"
        }),
    )
}

pub(super) async fn isolated_store(url: &str) -> (PgPool, PostgresSchema, PostgresEventStore) {
    let pool = PgPool::connect(url).await.unwrap();
    let name = format!("append_{}", uuid::Uuid::new_v4().simple());
    let schema = PostgresSchema::new(name).unwrap();
    run_migrations_in_schema(&pool, &schema).await.unwrap();
    let store = PostgresEventStore::with_schema(pool.clone(), schema.clone());
    (pool, schema, store)
}

pub(super) async fn drop_schema(pool: &PgPool, schema: &PostgresSchema) {
    sqlx::query(&format!(
        "DROP SCHEMA {} CASCADE",
        schema.prefix().trim_end_matches('.')
    ))
    .execute(pool)
    .await
    .unwrap();
}

/// Every row of every table a move writes, as JSON text, so two snapshots
/// compare whole rows (timestamps and ids included).
pub(super) async fn snapshot(
    pool: &PgPool,
    schema: &PostgresSchema,
) -> BTreeMap<&'static str, Vec<String>> {
    let mut rows = BTreeMap::new();
    for table in TABLES {
        let sql = format!(
            "SELECT row_to_json(r)::text FROM {}{table} r WHERE r.tenant = $1 ORDER BY 1",
            schema.prefix()
        );
        let found: Vec<(String,)> = sqlx::query_as(&sql)
            .bind(TENANT)
            .fetch_all(pool)
            .await
            .unwrap();
        rows.insert(table, found.into_iter().map(|(row,)| row).collect());
    }
    rows
}

pub(super) async fn index_rows(store: &PostgresEventStore, pool: &PgPool) -> Vec<(String, String)> {
    sqlx::query_as(&store.schema().qualify_sql(
        "SELECT field_name, field_value FROM {schema}entity_field_index \
         WHERE tenant = 't' AND entity_id = 'p1' ORDER BY field_name COLLATE \"C\"",
    ))
    .fetch_all(pool)
    .await
    .unwrap()
}

pub(super) async fn count(store: &PostgresEventStore, pool: &PgPool, table: &'static str) -> i64 {
    let sql = format!(
        "SELECT COUNT(*)::bigint FROM {}{table} WHERE tenant = 't'",
        store.schema().prefix()
    );
    sqlx::query_scalar(&sql).fetch_one(pool).await.unwrap()
}

/// The record's rows besides its events: one key, one vector, one
/// scheduled action, a timer, and its projection.
pub(super) struct Rows {
    pub(super) key: Vec<EntityKeyRow>,
    pub(super) vector: Vec<EntityVectorRow>,
    pub(super) scheduled: Vec<ScheduledActionRecord>,
    pub(super) timer: StateTimerRecord,
    pub(super) fields: serde_json::Value,
    pub(super) state: serde_json::Value,
}

impl Rows {
    pub(super) fn at(sequence_nr: u64, fields: serde_json::Value) -> Self {
        Self {
            key: vec![EntityKeyRow {
                key_name: "slug".into(),
                key_hash: format!("slug-{sequence_nr}"),
            }],
            vector: vec![EntityVectorRow {
                decl_name: "body".into(),
                model_tag: "m1".into(),
                vector: vec![sequence_nr as f32, 1.0],
            }],
            scheduled: vec![ScheduledActionRecord {
                tenant: TENANT.into(),
                entity_type: "Panel".into(),
                entity_id: "p1".into(),
                arm_id: format!("{sequence_nr}-0"),
                action: "Remind".into(),
                due_at: "2026-11-03T00:00:00Z".into(),
                armed_by: "{}".into(),
            }],
            timer: StateTimerRecord {
                tenant: TENANT.into(),
                entity_type: "Panel".into(),
                entity_id: "p1".into(),
                state: "Open".into(),
                arm_id: format!("Open-{sequence_nr}"),
                sequence_nr,
                started_at: "2026-11-02T00:00:00Z".into(),
                params: "{}".into(),
                armed_by: "{}".into(),
            },
            state: serde_json::json!({"status": "Open", "sequence_nr": sequence_nr}),
            fields,
        }
    }

    pub(super) fn commit(&self) -> CommitRows<'_> {
        CommitRows {
            key_rows: &self.key,
            vector_rows: &self.vector,
            reconcile_vectors: true,
            scheduled_actions: &self.scheduled,
            fired_scheduled_action: None,
            projection: Some(ProjectionRow::Upsert {
                status: "Open",
                fields: &self.fields,
                state: &self.state,
            }),
            state_timer: Some(StateTimerRow::Arm(&self.timer)),
        }
    }
}

pub(super) fn batched(
    persistence_id: &str,
    expected_sequence: u64,
    projection: Option<AppendProjection>,
) -> PersistenceAppend {
    PersistenceAppend {
        persistence_id: persistence_id.to_string(),
        expected_sequence,
        events: vec![envelope("Moved", serde_json::json!({}))],
        state_timer: None,
        projection,
    }
}

pub(super) fn kept(app: &str) -> Option<AppendProjection> {
    Some(AppendProjection::Upsert {
        status: "Kept".to_string(),
        fields: serde_json::json!({ "app": app }),
        state: serde_json::json!({}),
    })
}
