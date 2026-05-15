use std::borrow::Cow;
use std::marker::PhantomData;

use sqlx::postgres::{PgArguments, PgQueryResult, PgRow};
use sqlx::{Arguments, Encode, Executor, FromRow, Postgres, Type};
use tracing::Instrument as _;

#[derive(Clone, Debug)]
pub(super) struct TaggedSql {
    pub(super) text: Cow<'static, str>,
    pub(super) persistent: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct QuerySpanMetadata {
    pub(crate) operation: String,
    pub(crate) relation: Option<String>,
    pub(crate) resource: String,
    statement: String,
    database_service: String,
}

pub(crate) struct PostgresQuery {
    sql: TaggedSql,
    args: Result<PgArguments, sqlx::Error>,
}

impl PostgresQuery {
    pub(crate) fn new(sql: &'static str) -> Self {
        Self {
            sql: super::tag_static_sql(sql),
            args: Ok(PgArguments::default()),
        }
    }

    pub(crate) fn bind<'q, T>(mut self, value: T) -> Self
    where
        T: 'q + Encode<'q, Postgres> + Type<Postgres>,
    {
        if let Ok(arguments) = self.args.as_mut()
            && let Err(error) = <PgArguments as Arguments<'q>>::add(arguments, value)
        {
            self.args = Err(sqlx::Error::Encode(error));
        }
        self
    }

    pub(crate) async fn execute<'c, E>(self, executor: E) -> Result<PgQueryResult, sqlx::Error>
    where
        E: Executor<'c, Database = Postgres>,
    {
        let (sql, args, persistent, metadata) = self.into_parts()?;
        let span = postgres_span(&metadata);
        async move {
            sqlx::query_with::<Postgres, _>(&sql, args)
                .persistent(persistent)
                .execute(executor)
                .await
        }
        .instrument(span)
        .await
    }

    pub(crate) async fn fetch_all<'c, E>(self, executor: E) -> Result<Vec<PgRow>, sqlx::Error>
    where
        E: Executor<'c, Database = Postgres>,
    {
        let (sql, args, persistent, metadata) = self.into_parts()?;
        let span = postgres_span(&metadata);
        async move {
            sqlx::query_with::<Postgres, _>(&sql, args)
                .persistent(persistent)
                .fetch_all(executor)
                .await
        }
        .instrument(span)
        .await
    }

    pub(crate) async fn fetch_optional<'c, E>(
        self,
        executor: E,
    ) -> Result<Option<PgRow>, sqlx::Error>
    where
        E: Executor<'c, Database = Postgres>,
    {
        let (sql, args, persistent, metadata) = self.into_parts()?;
        let span = postgres_span(&metadata);
        async move {
            sqlx::query_with::<Postgres, _>(&sql, args)
                .persistent(persistent)
                .fetch_optional(executor)
                .await
        }
        .instrument(span)
        .await
    }

    fn into_parts(self) -> Result<(String, PgArguments, bool, QuerySpanMetadata), sqlx::Error> {
        let sql = self.sql.text.into_owned();
        let metadata = query_span_metadata(&sql);
        Ok((sql, self.args?, self.sql.persistent, metadata))
    }
}

pub(crate) struct PostgresQueryAs<O> {
    inner: PostgresQuery,
    output: PhantomData<O>,
}

impl<O> PostgresQueryAs<O>
where
    O: for<'r> FromRow<'r, PgRow> + Send + Unpin,
{
    pub(crate) fn new(sql: &'static str) -> Self {
        Self {
            inner: PostgresQuery::new(sql),
            output: PhantomData,
        }
    }

    pub(crate) fn bind<'q, T>(mut self, value: T) -> Self
    where
        T: 'q + Encode<'q, Postgres> + Type<Postgres>,
    {
        self.inner = self.inner.bind(value);
        self
    }

    pub(crate) async fn fetch_all<'c, E>(self, executor: E) -> Result<Vec<O>, sqlx::Error>
    where
        E: Executor<'c, Database = Postgres>,
    {
        let (sql, args, persistent, metadata) = self.inner.into_parts()?;
        let span = postgres_span(&metadata);
        async move {
            sqlx::query_as_with::<Postgres, O, _>(&sql, args)
                .persistent(persistent)
                .fetch_all(executor)
                .await
        }
        .instrument(span)
        .await
    }

    pub(crate) async fn fetch_optional<'c, E>(self, executor: E) -> Result<Option<O>, sqlx::Error>
    where
        E: Executor<'c, Database = Postgres>,
    {
        let (sql, args, persistent, metadata) = self.inner.into_parts()?;
        let span = postgres_span(&metadata);
        async move {
            sqlx::query_as_with::<Postgres, O, _>(&sql, args)
                .persistent(persistent)
                .fetch_optional(executor)
                .await
        }
        .instrument(span)
        .await
    }

    pub(crate) async fn fetch_one<'c, E>(self, executor: E) -> Result<O, sqlx::Error>
    where
        E: Executor<'c, Database = Postgres>,
    {
        let (sql, args, persistent, metadata) = self.inner.into_parts()?;
        let span = postgres_span(&metadata);
        async move {
            sqlx::query_as_with::<Postgres, O, _>(&sql, args)
                .persistent(persistent)
                .fetch_one(executor)
                .await
        }
        .instrument(span)
        .await
    }
}

pub(crate) struct PostgresQueryScalar<O> {
    inner: PostgresQuery,
    output: PhantomData<O>,
}

impl<O> PostgresQueryScalar<O>
where
    (O,): for<'r> FromRow<'r, PgRow>,
    O: Send + Unpin,
{
    pub(crate) fn new(sql: &'static str) -> Self {
        Self {
            inner: PostgresQuery::new(sql),
            output: PhantomData,
        }
    }

    pub(crate) fn bind<'q, T>(mut self, value: T) -> Self
    where
        T: 'q + Encode<'q, Postgres> + Type<Postgres>,
    {
        self.inner = self.inner.bind(value);
        self
    }

    pub(crate) async fn fetch_all<'c, E>(self, executor: E) -> Result<Vec<O>, sqlx::Error>
    where
        E: Executor<'c, Database = Postgres>,
    {
        let (sql, args, persistent, metadata) = self.inner.into_parts()?;
        let span = postgres_span(&metadata);
        async move {
            sqlx::query_scalar_with::<Postgres, O, _>(&sql, args)
                .persistent(persistent)
                .fetch_all(executor)
                .await
        }
        .instrument(span)
        .await
    }

    pub(crate) async fn fetch_one<'c, E>(self, executor: E) -> Result<O, sqlx::Error>
    where
        E: Executor<'c, Database = Postgres>,
    {
        let (sql, args, persistent, metadata) = self.inner.into_parts()?;
        let span = postgres_span(&metadata);
        async move {
            sqlx::query_scalar_with::<Postgres, O, _>(&sql, args)
                .persistent(persistent)
                .fetch_one(executor)
                .await
        }
        .instrument(span)
        .await
    }

    pub(crate) async fn fetch_optional<'c, E>(self, executor: E) -> Result<Option<O>, sqlx::Error>
    where
        E: Executor<'c, Database = Postgres>,
    {
        let (sql, args, persistent, metadata) = self.inner.into_parts()?;
        let span = postgres_span(&metadata);
        async move {
            sqlx::query_scalar_with::<Postgres, O, _>(&sql, args)
                .persistent(persistent)
                .fetch_optional(executor)
                .await
        }
        .instrument(span)
        .await
    }
}

fn postgres_span(metadata: &QuerySpanMetadata) -> tracing::Span {
    let span = tracing::info_span!(
        "postgres.query",
        otel.name = %metadata.resource,
        otel.kind = "client",
        span.kind = "client",
        db.system = "postgresql",
        db.operation = %metadata.operation,
        db.statement = %metadata.statement,
        db.collection.name = tracing::field::Empty,
        peer.service = %metadata.database_service,
    );
    if let Some(relation) = metadata.relation.as_deref() {
        span.record("db.collection.name", relation);
    }
    span
}

pub(crate) fn query_span_metadata(sql: &str) -> QuerySpanMetadata {
    let statement = compact_sql(strip_leading_sqlcommenter(sql));
    let operation = first_sql_token(&statement)
        .unwrap_or("QUERY")
        .to_ascii_uppercase();
    let relation = relation_for_operation(&statement, &operation);
    let resource = if let Some(relation) = relation.as_deref() {
        format!("postgres {operation} {relation}")
    } else {
        format!("postgres {operation}")
    };

    QuerySpanMetadata {
        operation,
        relation,
        resource,
        statement,
        database_service: database_service_name(),
    }
}

fn database_service_name() -> String {
    super::non_empty_env("DD_DBM_DATABASE_SERVICE")
        .or_else(|| super::non_empty_env("DD_DB_SERVICE"))
        .or_else(|| super::non_empty_env("DD_SERVICE").map(|service| format!("{service}-postgres")))
        .unwrap_or_else(|| "temper-postgres".to_string())
}

fn first_sql_token(sql: &str) -> Option<&str> {
    sql.split_whitespace()
        .next()
        .map(|token| token.trim_matches(SQL_TOKEN_TRIM))
        .filter(|token| !token.is_empty())
}

fn relation_for_operation(sql: &str, operation: &str) -> Option<String> {
    let tokens = sql_tokens(sql);
    match operation {
        "SELECT" => token_after(&tokens, "FROM"),
        "INSERT" => token_after(&tokens, "INTO"),
        "UPDATE" => tokens.get(1).cloned(),
        "DELETE" => token_after(&tokens, "FROM"),
        _ => None,
    }
    .and_then(clean_relation)
}

fn token_after(tokens: &[String], needle: &str) -> Option<String> {
    tokens
        .windows(2)
        .find(|window| window[0].eq_ignore_ascii_case(needle))
        .map(|window| window[1].clone())
}

fn sql_tokens(sql: &str) -> Vec<String> {
    sql.split_whitespace()
        .map(|token| token.trim_matches(SQL_TOKEN_TRIM))
        .filter(|token| !token.is_empty())
        .map(str::to_string)
        .collect()
}

const SQL_TOKEN_TRIM: &[char] = &[',', '(', ')', ';', '"', '\'', '`', '[', ']'];

fn clean_relation(token: String) -> Option<String> {
    let cleaned = token
        .trim_matches(SQL_TOKEN_TRIM)
        .trim_end_matches(',')
        .split('.')
        .rfind(|segment| !segment.is_empty())
        .unwrap_or("")
        .trim_matches(SQL_TOKEN_TRIM)
        .to_string();
    (!cleaned.is_empty()).then_some(cleaned)
}

fn strip_leading_sqlcommenter(sql: &str) -> &str {
    let trimmed = sql.trim_start();
    if let Some(rest) = trimmed.strip_prefix("/*")
        && let Some(end) = rest.find("*/")
    {
        return rest[(end + 2)..].trim_start();
    }
    trimmed
}

fn compact_sql(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}
