//! Native bounded query-page reads for the Postgres query projection.

use temper_runtime::persistence::PersistenceError;

use crate::platform::{postgres_placeholders, storage_error};
use crate::store::PostgresEventStore;

impl PostgresEventStore {
    #[expect(
        clippy::too_many_arguments,
        reason = "bounded projection page query boundary"
    )]
    pub async fn query_field_index_page(
        &self,
        tenant: &str,
        entity_type: &str,
        where_clause: &str,
        params: Vec<String>,
        order_by: &[(String, bool)],
        skip: usize,
        top: usize,
        include_count: bool,
    ) -> Result<(Vec<String>, Option<usize>), PersistenceError> {
        if top == 0 {
            return Ok((Vec::new(), if include_count { Some(0) } else { None }));
        }

        let clause = postgres_placeholders(where_clause, params.len() + 2);
        let order_sql = postgres_query_field_order_sql(order_by);
        let limit_param = params.len() + 3;
        let offset_param = params.len() + 4;
        let sql = postgres_query_field_page_sql(
            self.schema.prefix(),
            &clause,
            &order_sql,
            limit_param,
            offset_param,
            include_count,
        );
        let tagged_sql = crate::dbm::tag_sql(&sql);
        if !include_count {
            let mut query = sqlx::query_scalar::<_, String>(tagged_sql.as_ref())
                .bind(tenant)
                .bind(entity_type);
            for param in params.iter().cloned() {
                query = query.bind(param);
            }
            query = query
                .bind(top.min(i64::MAX as usize) as i64)
                .bind(skip.min(i64::MAX as usize) as i64);

            let entity_ids = query.fetch_all(self.pool()).await.map_err(storage_error)?;
            return Ok((entity_ids, None));
        }

        let mut query = sqlx::query_as::<_, (String, i64)>(tagged_sql.as_ref())
            .bind(tenant)
            .bind(entity_type);
        for param in params.iter().cloned() {
            query = query.bind(param);
        }
        query = query
            .bind(top.min(i64::MAX as usize) as i64)
            .bind(skip.min(i64::MAX as usize) as i64);

        let rows = query.fetch_all(self.pool()).await.map_err(storage_error)?;
        let total_count = if include_count {
            if let Some((_, count)) = rows.first() {
                Some((*count).max(0) as usize)
            } else {
                Some(
                    self.query_field_index_count(tenant, entity_type, &clause, params)
                        .await?,
                )
            }
        } else {
            None
        };
        let entity_ids = rows
            .into_iter()
            .map(|(entity_id, _)| entity_id)
            .collect::<Vec<_>>();
        Ok((entity_ids, total_count))
    }

    async fn query_field_index_count(
        &self,
        tenant: &str,
        entity_type: &str,
        clause: &str,
        params: Vec<String>,
    ) -> Result<usize, PersistenceError> {
        let schema = self.schema.prefix();
        let index_cte = field_index_cte(schema);
        let sql = format!(
            "{index_cte}SELECT COUNT(*) \
             FROM {schema}entity_catalog \
             WHERE tenant = $1 AND entity_type = $2 AND ({clause})"
        );
        let tagged_sql = crate::dbm::tag_sql(&sql);
        let mut query = sqlx::query_scalar::<_, i64>(tagged_sql.as_ref())
            .bind(tenant)
            .bind(entity_type);
        for param in params {
            query = query.bind(param);
        }
        let count = query.fetch_one(self.pool()).await.map_err(storage_error)?;
        Ok(count.max(0) as usize)
    }
}

fn postgres_query_field_page_sql(
    schema: &str,
    clause: &str,
    order_sql: &str,
    limit_param: usize,
    offset_param: usize,
    include_count: bool,
) -> String {
    let select = if include_count {
        "entity_id, COUNT(*) OVER() AS total_count"
    } else {
        "entity_id"
    };
    let index_cte = field_index_cte(schema);
    format!(
        "{index_cte}SELECT {select} \
         FROM {schema}entity_catalog \
         WHERE tenant = $1 AND entity_type = $2 AND ({clause}) \
         ORDER BY {order_sql} \
         LIMIT ${limit_param} OFFSET ${offset_param}"
    )
}

// Generated filters and their completeness guards refer to these query
// relations. Bind both to the selected physical tables without rewriting the
// fragment or its literals. With no schema selected, retain the original SQL.
pub(crate) fn field_index_cte(schema: &str) -> String {
    if schema.is_empty() {
        return String::new();
    }
    format!(
        "WITH entity_field_index AS (SELECT * FROM {schema}entity_field_index WHERE tenant = $1 AND entity_type = $2), \
         entity_catalog AS (SELECT * FROM {schema}entity_catalog WHERE tenant = $1 AND entity_type = $2) "
    )
}

fn postgres_query_field_order_sql(order_by: &[(String, bool)]) -> String {
    let mut clauses = Vec::new();
    for (field_name, descending) in order_by {
        let direction = if *descending { "DESC" } else { "ASC" };
        let nulls = if *descending {
            "NULLS FIRST"
        } else {
            "NULLS LAST"
        };
        if field_name == "entity_id" || field_name == "Id" || field_name == "id" {
            clauses.push(format!("entity_id {direction}"));
        } else if field_name == "status" || field_name == "Status" {
            clauses.push(format!("status {direction} {nulls}"));
        } else {
            let field = postgres_string_literal(field_name);
            clauses.push(format!(
                "CASE WHEN jsonb_typeof(fields -> {field}) = 'number' \
                 THEN (fields ->> {field})::numeric END {direction} {nulls}"
            ));
            clauses.push(format!(
                "CASE WHEN jsonb_typeof(fields -> {field}) <> 'number' \
                 THEN fields ->> {field} END {direction} {nulls}"
            ));
        }
    }
    clauses.push("entity_id ASC".to_string());
    clauses.join(", ")
}

fn postgres_string_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}
