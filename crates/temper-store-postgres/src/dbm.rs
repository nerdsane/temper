use std::borrow::Cow;

mod query;
#[cfg(test)]
pub(crate) use query::query_span_metadata;
pub(crate) use query::{PostgresQuery, PostgresQueryAs, PostgresQueryScalar};

macro_rules! postgres_query {
    ($sql:expr $(,)?) => {
        $crate::dbm::PostgresQuery::new($sql)
    };
}

macro_rules! postgres_query_as {
    ($sql:expr $(,)?) => {
        $crate::dbm::PostgresQueryAs::new($sql)
    };
}

macro_rules! postgres_query_scalar {
    ($sql:expr $(,)?) => {
        $crate::dbm::PostgresQueryScalar::new($sql)
    };
}

pub(crate) use postgres_query;
pub(crate) use postgres_query_as;
pub(crate) use postgres_query_scalar;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DbmSqlCommentConfig {
    mode: DbmPropagationMode,
    database_service: String,
    parent_service: String,
    env: Option<String>,
    version: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DbmPropagationMode {
    Disabled,
    Service,
    Full,
}

impl DbmSqlCommentConfig {
    pub(crate) fn disabled() -> Self {
        Self {
            mode: DbmPropagationMode::Disabled,
            database_service: String::new(),
            parent_service: String::new(),
            env: None,
            version: None,
        }
    }

    pub(crate) fn service(
        database_service: impl Into<String>,
        parent_service: impl Into<String>,
        env: Option<impl Into<String>>,
        version: Option<impl Into<String>>,
    ) -> Self {
        Self {
            mode: DbmPropagationMode::Service,
            database_service: database_service.into(),
            parent_service: parent_service.into(),
            env: env.map(Into::into),
            version: version.map(Into::into),
        }
    }

    pub(crate) fn full(
        database_service: impl Into<String>,
        parent_service: impl Into<String>,
        env: Option<impl Into<String>>,
        version: Option<impl Into<String>>,
    ) -> Self {
        Self {
            mode: DbmPropagationMode::Full,
            database_service: database_service.into(),
            parent_service: parent_service.into(),
            env: env.map(Into::into),
            version: version.map(Into::into),
        }
    }

    fn enabled(&self) -> bool {
        !matches!(self.mode, DbmPropagationMode::Disabled)
    }

    fn trace_correlation_enabled(&self) -> bool {
        matches!(self.mode, DbmPropagationMode::Full)
    }

    fn from_env() -> Self {
        let mode = std::env::var("DD_DBM_PROPAGATION_MODE")
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !matches!(mode.as_str(), "service" | "full") {
            return Self::disabled();
        }

        let parent_service = non_empty_env("DD_SERVICE").unwrap_or_else(|| "temper".to_string());
        let database_service = non_empty_env("DD_DBM_DATABASE_SERVICE")
            .or_else(|| non_empty_env("DD_DB_SERVICE"))
            .unwrap_or_else(|| format!("{parent_service}-postgres"));

        let env = non_empty_env("DD_ENV");
        let version = non_empty_env("DD_VERSION");
        if mode == "full" {
            Self::full(database_service, parent_service, env, version)
        } else {
            Self::service(database_service, parent_service, env, version)
        }
    }
}

pub(crate) fn tag_sql(sql: &str) -> Cow<'_, str> {
    let config = DbmSqlCommentConfig::from_env();
    if !config.enabled() {
        return Cow::Borrowed(sql);
    }
    Cow::Owned(tag_sql_with_config_and_traceparent(
        sql,
        &config,
        current_traceparent_header().as_deref(),
    ))
}

fn tag_static_sql(sql: &'static str) -> query::TaggedSql {
    let config = DbmSqlCommentConfig::from_env();
    if !config.enabled() {
        return query::TaggedSql {
            text: Cow::Borrowed(sql),
            persistent: true,
        };
    }

    query::TaggedSql {
        text: Cow::Owned(tag_sql_with_config_and_traceparent(
            sql,
            &config,
            current_traceparent_header().as_deref(),
        )),
        persistent: !config.trace_correlation_enabled(),
    }
}

#[cfg(test)]
pub(crate) fn tag_sql_with_config(sql: &str, config: &DbmSqlCommentConfig) -> String {
    tag_sql_with_config_and_traceparent(sql, config, None)
}

pub(crate) fn tag_sql_with_config_and_traceparent(
    sql: &str,
    config: &DbmSqlCommentConfig,
    traceparent: Option<&str>,
) -> String {
    if !config.enabled() {
        return sql.to_string();
    }

    let mut tags = vec![
        ("dddbs", config.database_service.as_str()),
        ("ddps", config.parent_service.as_str()),
    ];
    if let Some(env) = config.env.as_deref().filter(|value| !value.is_empty()) {
        tags.push(("dde", env));
    }
    if let Some(version) = config.version.as_deref().filter(|value| !value.is_empty()) {
        tags.push(("ddpv", version));
    }
    if config.trace_correlation_enabled()
        && let Some(traceparent) = traceparent.filter(|value| !value.is_empty())
    {
        tags.push(("traceparent", traceparent));
    }

    let rendered_tags = tags
        .into_iter()
        .map(|(key, value)| format!("{key}='{}'", escape_sqlcommenter_value(value)))
        .collect::<Vec<_>>()
        .join(",");
    format!("/*{rendered_tags}*/ {sql}")
}

fn current_traceparent_header() -> Option<String> {
    use opentelemetry::trace::TraceContextExt as _;
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;

    let span_context = tracing::Span::current()
        .context()
        .span()
        .span_context()
        .clone();
    if !span_context.is_valid() {
        return None;
    }
    let flags = if span_context.trace_flags().is_sampled() {
        "01"
    } else {
        "00"
    };
    Some(format!(
        "00-{}-{}-{}",
        span_context.trace_id(),
        span_context.span_id(),
        flags
    ))
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn escape_sqlcommenter_value(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b':') {
            escaped.push(byte as char);
        } else {
            escaped.push('%');
            escaped.push(nibble_to_hex(byte >> 4));
            escaped.push(nibble_to_hex(byte & 0x0f));
        }
    }
    escaped
}

fn nibble_to_hex(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        10..=15 => (b'A' + (nibble - 10)) as char,
        _ => unreachable!("nibble outside hex range"),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn disabled_mode_leaves_sql_untagged() {
        let sql = "SELECT 1";
        let config = super::DbmSqlCommentConfig::disabled();

        assert_eq!(super::tag_sql_with_config(sql, &config), sql);
    }

    #[test]
    fn service_mode_prepends_sqlcommenter_tags_for_datadog_dbm() {
        let sql = "SELECT sequence_nr FROM snapshots WHERE tenant = $1";
        let config = super::DbmSqlCommentConfig::service(
            "temperpaw-postgres",
            "temperpaw",
            Some("prod"),
            Some("abc123"),
        );

        let tagged = super::tag_sql_with_config(sql, &config);

        assert!(tagged.starts_with("/*"));
        assert!(tagged.contains("dddbs='temperpaw-postgres'"));
        assert!(tagged.contains("ddps='temperpaw'"));
        assert!(tagged.contains("dde='prod'"));
        assert!(tagged.contains("ddpv='abc123'"));
        assert!(tagged.ends_with(sql));
    }

    #[test]
    fn full_mode_adds_traceparent_for_datadog_dbm_trace_correlation() {
        let sql = "SELECT sequence_nr FROM snapshots WHERE tenant = $1";
        let config = super::DbmSqlCommentConfig::full(
            "temperpaw-postgres",
            "temperpaw",
            Some("prod"),
            Some("abc123"),
        );

        let tagged = super::tag_sql_with_config_and_traceparent(
            sql,
            &config,
            Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"),
        );

        assert!(
            tagged
                .contains("traceparent='00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01'")
        );
        assert!(tagged.ends_with(sql));
    }

    #[test]
    fn postgres_span_resource_is_stable_and_table_oriented() {
        let metadata = super::query_span_metadata(
            "/*ddps='temperpaw'*/ INSERT INTO snapshots (tenant, entity_type) VALUES ($1, $2)",
        );

        assert_eq!(metadata.operation, "INSERT");
        assert_eq!(metadata.relation.as_deref(), Some("snapshots"));
        assert_eq!(metadata.resource, "postgres INSERT snapshots");
    }

    #[test]
    fn sqlcommenter_values_are_percent_escaped() {
        let sql = "SELECT 1";
        let config = super::DbmSqlCommentConfig::service(
            "temper paw/postgres",
            "temper'paw",
            Some("prod east"),
            Some("2026.05.12+build"),
        );

        let tagged = super::tag_sql_with_config(sql, &config);

        assert!(tagged.contains("dddbs='temper%20paw%2Fpostgres'"));
        assert!(tagged.contains("ddps='temper%27paw'"));
        assert!(tagged.contains("dde='prod%20east'"));
        assert!(tagged.contains("ddpv='2026.05.12%2Bbuild'"));
    }
}
