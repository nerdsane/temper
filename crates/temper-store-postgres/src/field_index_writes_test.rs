//! How many `entity_field_index` rows a move writes on PostgreSQL.
//!
//! Every row a write inserts, updates or deletes is a new row version and a
//! WAL record. A trigger records those row writes, so each test reads what a
//! move writes from the field index itself. A move that sets one field of a
//! record writes one index row, whatever else it indexes, and a status change
//! writes none: the status lives in the catalog row.

use std::collections::BTreeMap;

use serde_json::{Value, json};
use sqlx::PgPool;

use super::PostgresEventStore;
use crate::PostgresSchema;
use crate::migration::run_migrations_in_schema;

const ENTITY_TYPE: &str = "Ticket";
/// Scalar fields on the test record, one index row each.
const FIELD_COUNT: usize = 20;

/// A record with `FIELD_COUNT` scalar fields, `Field00` to `Field19`, each
/// holding `value-<n>`, with `changes` applied on top (`null` removes one).
fn record_fields(changes: &[(&str, Value)]) -> Value {
    let mut fields = serde_json::Map::new();
    for n in 0..FIELD_COUNT {
        fields.insert(format!("Field{n:02}"), json!(format!("value-{n}")));
    }
    for (name, value) in changes {
        if value.is_null() {
            fields.remove(*name);
        } else {
            fields.insert((*name).to_string(), value.clone());
        }
    }
    Value::Object(fields)
}

async fn project(
    store: &PostgresEventStore,
    [tenant, entity_id]: [&str; 2],
    status: &str,
    fields: &Value,
    seq: u64,
) {
    store
        .upsert_query_projection(tenant, ENTITY_TYPE, entity_id, status, fields, seq)
        .await
        .expect("projection upsert");
}

/// Record every row `entity_field_index` gains, changes or loses. The store
/// works in its own schema, so the trigger sees only this test's writes.
async fn log_field_index_writes(pool: &PgPool, schema: &PostgresSchema) {
    for sql in [
        "CREATE TABLE {schema}field_index_writes (op TEXT NOT NULL)",
        "CREATE FUNCTION {schema}log_field_index_write() RETURNS trigger \
         LANGUAGE plpgsql AS $$ BEGIN \
           INSERT INTO {schema}field_index_writes VALUES (TG_OP); \
           RETURN NULL; \
         END $$",
        "CREATE TRIGGER field_index_writes \
         AFTER INSERT OR UPDATE OR DELETE ON {schema}entity_field_index \
         FOR EACH ROW EXECUTE FUNCTION {schema}log_field_index_write()",
    ] {
        crate::dbm::postgres_query!(schema, sql)
            .execute(pool)
            .await
            .expect("set up write log");
    }
}

/// Row writes since the last call, by kind, so a failure shows what was
/// written.
async fn take_writes(pool: &PgPool, schema: &PostgresSchema) -> BTreeMap<String, i64> {
    let writes: Vec<(String, i64)> = crate::dbm::postgres_query_as!(
        schema,
        "SELECT op, count(*) FROM {schema}field_index_writes GROUP BY op",
    )
    .fetch_all(pool)
    .await
    .expect("read write log");
    crate::dbm::postgres_query!(schema, "DELETE FROM {schema}field_index_writes")
        .execute(pool)
        .await
        .expect("clear write log");
    writes.into_iter().collect()
}

fn total(writes: &BTreeMap<String, i64>) -> i64 {
    writes.values().sum()
}

/// The record's index rows as `field_name -> field_value`.
async fn index_rows(
    pool: &PgPool,
    schema: &PostgresSchema,
    [tenant, entity_id]: [&str; 2],
) -> BTreeMap<String, Option<String>> {
    let rows: Vec<(String, Option<String>)> = crate::dbm::postgres_query_as!(
        schema,
        "SELECT field_name, field_value FROM {schema}entity_field_index \
         WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3",
    )
    .bind(tenant)
    .bind(ENTITY_TYPE)
    .bind(entity_id)
    .fetch_all(pool)
    .await
    .expect("read index rows");
    rows.into_iter().collect()
}

/// How many of the record's index rows carry a status.
async fn rows_with_status(
    pool: &PgPool,
    schema: &PostgresSchema,
    [tenant, entity_id]: [&str; 2],
) -> i64 {
    crate::dbm::postgres_query_scalar!(
        schema,
        "SELECT count(*) FROM {schema}entity_field_index \
         WHERE tenant = $1 AND entity_type = $2 AND entity_id = $3 AND status IS NOT NULL",
    )
    .bind(tenant)
    .bind(ENTITY_TYPE)
    .bind(entity_id)
    .fetch_one(pool)
    .await
    .expect("count rows with a status")
}

/// The index rows a record with `fields` should have: one value per field.
fn expected_rows(fields: &Value) -> BTreeMap<String, Option<String>> {
    fields
        .as_object()
        .expect("fields object")
        .iter()
        .map(|(name, value)| (name.clone(), value.as_str().map(str::to_string)))
        .collect()
}

/// Each kind of move writes only the index rows it changes, and leaves the
/// rows exactly as a full rewrite would.
#[test]
fn a_projection_writes_only_the_index_rows_it_changes() {
    let Some(database_url) = temper_test_resource::env_var("DATABASE_URL") else {
        return;
    };

    sqlx::test_block_on(async {
        let pool = PgPool::connect(&database_url).await.unwrap();
        let schema_name = format!("temper_fiw_{}", uuid::Uuid::new_v4().simple());
        let schema = PostgresSchema::new(&schema_name).unwrap();
        run_migrations_in_schema(&pool, &schema).await.unwrap();
        let store = PostgresEventStore::with_schema(pool.clone(), schema.clone());
        log_field_index_writes(&pool, &schema).await;
        let tenant = format!("tenant-{}", uuid::Uuid::new_v4());
        let ticket = [tenant.as_str(), "ticket-1"];
        let created = record_fields(&[]);
        project(&store, ticket, "Open", &created, 1).await;
        let writes = take_writes(&pool, &schema).await;
        assert_eq!(
            total(&writes),
            FIELD_COUNT as i64,
            "the first projection writes one row per indexed field: {writes:?}"
        );
        assert_eq!(
            index_rows(&pool, &schema, ticket).await,
            expected_rows(&created)
        );

        // (what the move does, the fields it sets, the status it leaves, the index
        // rows it writes). Each move keeps what the moves before it set.
        let moves = [
            (
                "sets one field",
                vec![("Field07", json!("edited"))],
                "Open",
                1,
            ),
            (
                "sets three fields",
                vec![
                    ("Field01", json!("one")),
                    ("Field02", json!("two")),
                    ("Field03", json!("three")),
                ],
                "Open",
                3,
            ),
            ("adds a field", vec![("Assignee", json!("ana"))], "Open", 1),
            ("removes a field", vec![("Field19", Value::Null)], "Open", 1),
            ("changes status", vec![], "Closed", 0),
            // The Factory's usual move: a new status and a field or two.
            (
                "changes status and sets two fields",
                vec![("Field04", json!("four")), ("Field05", json!("five"))],
                "Review",
                2,
            ),
        ];
        let mut changes = Vec::new();
        for (seq, (what, sets, status, expected)) in (2..).zip(moves) {
            changes.extend(sets);
            let fields = record_fields(&changes);
            project(&store, ticket, status, &fields, seq).await;
            let writes = take_writes(&pool, &schema).await;
            assert_eq!(
                total(&writes),
                expected,
                "field-index rows written by a move that {what}: {writes:?}"
            );
            assert_eq!(
                index_rows(&pool, &schema, ticket).await,
                expected_rows(&fields),
                "index rows after a move that {what}"
            );
        }

        // No row copies the status, so a status change has no index rows to
        // write. A filter finds the record by the status in its catalog row,
        // and by a field through the index.
        assert_eq!(rows_with_status(&pool, &schema, ticket).await, 0);
        for (status, found) in [("Review", true), ("Open", false)] {
            let ids = store
                .query_field_index(
                    ticket[0],
                    ENTITY_TYPE,
                    "status = ?3",
                    vec![status.to_string()],
                )
                .await
                .expect("query by status");
            assert_eq!(
                ids.contains(&ticket[1].to_string()),
                found,
                "status {status}: {ids:?}"
            );
        }
        let ids = store
            .query_field_index(
                ticket[0],
                ENTITY_TYPE,
                "entity_id IN (SELECT entity_id FROM entity_field_index \
                 WHERE tenant = ?1 AND entity_type = ?2 AND field_name = ?3 AND field_value = ?4)",
                vec!["Field04".to_string(), "four".to_string()],
            )
            .await
            .expect("query by field");
        assert_eq!(ids, vec![ticket[1].to_string()]);

        crate::dbm::postgres_query!(format!("DROP SCHEMA \"{schema_name}\" CASCADE"))
            .execute(&pool)
            .await
            .unwrap();
    });
}
