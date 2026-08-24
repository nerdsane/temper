//! Native query-plane page reads over the durable Turso catalog.

use temper_runtime::persistence::{
    PersistenceError, QueryProjectionOrder, QueryProjectionOrderTarget, storage_error,
};
use tracing::instrument;

use super::TursoEventStore;

impl TursoEventStore {
    /// Return a bounded page of entity IDs matching a translated query-plane filter.
    ///
    /// The returned count is the storage candidate count, before Cedar row
    /// authorization. Server read code must still materialize and authorize
    /// rows before using the count as an OData result count.
    #[expect(
        clippy::too_many_arguments,
        reason = "bounded projection page query boundary"
    )]
    #[instrument(skip_all, fields(
        otel.name = "turso.query_field_index_page",
        tenant, entity_type,
    ))]
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

        let conn = self.configured_connection().await?;
        let order_sql = turso_query_field_order_sql(order_by);
        let limit_param = params.len() + 3;
        let offset_param = params.len() + 4;
        let sql = turso_query_field_page_sql(
            where_clause,
            &order_sql,
            limit_param,
            offset_param,
            include_count,
        );

        let mut all_params: Vec<libsql::Value> = vec![
            libsql::Value::from(tenant.to_string()),
            libsql::Value::from(entity_type.to_string()),
        ];
        all_params.extend(params.iter().cloned().map(libsql::Value::from));
        all_params.push(libsql::Value::from(top.min(i64::MAX as usize) as i64));
        all_params.push(libsql::Value::from(skip.min(i64::MAX as usize) as i64));

        let mut rows = conn
            .query(&sql, libsql::params_from_iter(all_params))
            .await
            .map_err(storage_error)?;

        let mut ids = Vec::new();
        let mut total_count = None;
        while let Some(row) = rows.next().await.map_err(storage_error)? {
            let entity_id: String = row.get(0).map_err(storage_error)?;
            if include_count && total_count.is_none() {
                let count: i64 = row.get(1).map_err(storage_error)?;
                total_count = Some(count.max(0) as usize);
            }
            ids.push(entity_id);
        }

        if include_count && total_count.is_none() {
            total_count = Some(
                self.query_field_index_count(tenant, entity_type, where_clause, params)
                    .await?,
            );
        }

        Ok((ids, total_count))
    }

    async fn query_field_index_count(
        &self,
        tenant: &str,
        entity_type: &str,
        where_clause: &str,
        params: Vec<String>,
    ) -> Result<usize, PersistenceError> {
        let conn = self.configured_connection().await?;
        let sql = format!(
            "SELECT COUNT(*) \
             FROM entity_catalog \
             WHERE tenant = ?1 AND entity_type = ?2 AND ({where_clause})"
        );
        let mut all_params: Vec<libsql::Value> = vec![
            libsql::Value::from(tenant.to_string()),
            libsql::Value::from(entity_type.to_string()),
        ];
        all_params.extend(params.into_iter().map(libsql::Value::from));
        let count: i64 = conn
            .query(&sql, libsql::params_from_iter(all_params))
            .await
            .map_err(storage_error)?
            .next()
            .await
            .map_err(storage_error)?
            .and_then(|row| row.get(0).ok())
            .unwrap_or(0);
        Ok(count.max(0) as usize)
    }
}

fn turso_query_field_page_sql(
    where_clause: &str,
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
         WHERE tenant = ?1 AND entity_type = ?2 AND ({where_clause}) \
         ORDER BY {order_sql} \
         LIMIT ?{limit_param} OFFSET ?{offset_param}"
    )
}

fn turso_query_field_order_sql(order_by: &[QueryProjectionOrder]) -> String {
    let mut clauses = Vec::new();
    for order in order_by {
        let direction = if order.descending { "DESC" } else { "ASC" };
        let null_direction = if order.descending { "DESC" } else { "ASC" };
        match &order.target {
            QueryProjectionOrderTarget::EntityId => clauses.push(format!("entity_id {direction}")),
            QueryProjectionOrderTarget::Status => {
                clauses.push(format!("status IS NULL {null_direction}"));
                clauses.push(format!("status {direction}"));
            }
            QueryProjectionOrderTarget::EntityCommitSequence => {
                clauses.push(format!("sequence_nr {direction}"));
            }
            QueryProjectionOrderTarget::Property(field_name) => {
                let path = turso_json_path_literal(field_name);
                clauses.push(format!(
                    "json_extract(fields, {path}) IS NULL {null_direction}"
                ));
                clauses.push(format!(
                    "CASE WHEN json_type(fields, {path}) IN ('integer', 'real') \
                 THEN CAST(json_extract(fields, {path}) AS REAL) END IS NULL ASC"
                ));
                clauses.push(format!(
                    "CASE WHEN json_type(fields, {path}) IN ('integer', 'real') \
                 THEN CAST(json_extract(fields, {path}) AS REAL) END {direction}"
                ));
                clauses.push(format!(
                    "CASE WHEN json_type(fields, {path}) NOT IN ('integer', 'real') \
                 THEN json_extract(fields, {path}) END IS NULL ASC"
                ));
                clauses.push(format!(
                    "CASE WHEN json_type(fields, {path}) NOT IN ('integer', 'real') \
                 THEN json_extract(fields, {path}) END {direction}"
                ));
            }
        }
    }
    clauses.push("entity_id ASC".to_string());
    clauses.join(", ")
}

fn turso_json_path_literal(field_name: &str) -> String {
    let escaped = field_name
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\'', "''");
    format!("'$.\"{escaped}\"'")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_count_page_sql_does_not_compute_window_count() {
        let sql = turso_query_field_page_sql("entity_id = ?3", "entity_id ASC", 4, 5, false);

        assert!(sql.starts_with("SELECT entity_id FROM entity_catalog"));
        assert!(!sql.contains("COUNT(*) OVER()"));
    }

    #[test]
    fn count_page_sql_computes_count_only_when_requested() {
        let sql = turso_query_field_page_sql("entity_id = ?3", "entity_id ASC", 4, 5, true);

        assert!(sql.contains("COUNT(*) OVER() AS total_count"));
    }
}
