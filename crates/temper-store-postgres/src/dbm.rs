use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

static TAGGED_STATIC_SQL: LazyLock<Mutex<HashMap<String, &'static str>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

macro_rules! postgres_query {
    ($sql:expr $(,)?) => {
        sqlx::query($crate::dbm::tag_static_sql($sql))
    };
}

macro_rules! postgres_query_as {
    ($sql:expr $(,)?) => {
        sqlx::query_as($crate::dbm::tag_static_sql($sql))
    };
}

macro_rules! postgres_query_scalar {
    ($sql:expr $(,)?) => {
        sqlx::query_scalar($crate::dbm::tag_static_sql($sql))
    };
}

pub(crate) use postgres_query;
pub(crate) use postgres_query_as;
pub(crate) use postgres_query_scalar;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DbmSqlCommentConfig {
    enabled: bool,
    database_service: String,
    parent_service: String,
    env: Option<String>,
    version: Option<String>,
}

impl DbmSqlCommentConfig {
    pub(crate) fn disabled() -> Self {
        Self {
            enabled: false,
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
            enabled: true,
            database_service: database_service.into(),
            parent_service: parent_service.into(),
            env: env.map(Into::into),
            version: version.map(Into::into),
        }
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

        Self::service(
            database_service,
            parent_service,
            non_empty_env("DD_ENV"),
            non_empty_env("DD_VERSION"),
        )
    }
}

pub(crate) fn tag_sql(sql: &str) -> Cow<'_, str> {
    let config = DbmSqlCommentConfig::from_env();
    if !config.enabled {
        return Cow::Borrowed(sql);
    }
    Cow::Owned(tag_sql_with_config(sql, &config))
}

pub(crate) fn tag_static_sql(sql: &'static str) -> &'static str {
    let config = DbmSqlCommentConfig::from_env();
    if !config.enabled {
        return sql;
    }

    let tagged = tag_sql_with_config(sql, &config);
    let mut cache = TAGGED_STATIC_SQL
        .lock()
        .expect("tagged static SQL cache lock poisoned");
    if let Some(cached) = cache.get(&tagged) {
        return cached;
    }

    let leaked: &'static str = Box::leak(tagged.clone().into_boxed_str());
    cache.insert(tagged, leaked);
    leaked
}

pub(crate) fn tag_sql_with_config(sql: &str, config: &DbmSqlCommentConfig) -> String {
    if !config.enabled {
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

    let rendered_tags = tags
        .into_iter()
        .map(|(key, value)| format!("{key}='{}'", escape_sqlcommenter_value(value)))
        .collect::<Vec<_>>()
        .join(",");
    format!("/*{rendered_tags}*/ {sql}")
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
