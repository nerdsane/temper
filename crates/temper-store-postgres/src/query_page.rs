//! Native bounded query-page reads for the Postgres query projection.

use temper_runtime::persistence::{
    PersistenceError, QueryProjectionOrder, QueryProjectionOrderTarget,
};

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
        order_by: &[QueryProjectionOrder],
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
        let sql = format!(
            "SELECT COUNT(*) \
             FROM entity_catalog \
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
    format!(
        "SELECT {select} \
         FROM entity_catalog \
         WHERE tenant = $1 AND entity_type = $2 AND ({clause}) \
         ORDER BY {order_sql} \
         LIMIT ${limit_param} OFFSET ${offset_param}"
    )
}

fn postgres_query_field_order_sql(order_by: &[QueryProjectionOrder]) -> String {
    let mut clauses = Vec::new();
    for order in order_by {
        let direction = if order.descending { "DESC" } else { "ASC" };
        let nulls = if order.descending {
            "NULLS FIRST"
        } else {
            "NULLS LAST"
        };
        match &order.target {
            QueryProjectionOrderTarget::EntityId => clauses.push(format!("entity_id {direction}")),
            QueryProjectionOrderTarget::Status => {
                clauses.push(format!("status {direction} {nulls}"));
            }
            QueryProjectionOrderTarget::EntityCommitSequence => {
                clauses.push(format!("sequence_nr {direction}"));
            }
            QueryProjectionOrderTarget::Property(field_name) => {
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
    }
    clauses.push("entity_id ASC".to_string());
    clauses.join(", ")
}

fn postgres_string_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_count_page_sql_does_not_compute_window_count() {
        let sql = postgres_query_field_page_sql("entity_id = $3", "entity_id ASC", 4, 5, false);

        assert!(sql.starts_with("SELECT entity_id FROM entity_catalog"));
        assert!(!sql.contains("COUNT(*) OVER()"));
    }

    #[test]
    fn count_page_sql_computes_count_only_when_requested() {
        let sql = postgres_query_field_page_sql("entity_id = $3", "entity_id ASC", 4, 5, true);

        assert!(sql.contains("COUNT(*) OVER() AS total_count"));
    }
}
