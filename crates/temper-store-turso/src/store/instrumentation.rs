//! Turso query latency instrumentation — histogram recording and instrumented
//! connection wrapper.
//!
//! Exported for use by all `store` sub-modules via `super::instrumentation`.

use std::ops::Deref;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::Histogram;

fn turso_query_duration_histogram() -> &'static Histogram<f64> {
    static HISTOGRAM: OnceLock<Histogram<f64>> = OnceLock::new();
    HISTOGRAM.get_or_init(|| {
        global::meter("temper-store-turso")
            .f64_histogram("temper_turso_query_duration")
            .with_unit("ms")
            .with_description("Duration of Turso/libSQL query and execute calls.")
            .build()
    })
}

pub(super) fn record_turso_query_duration(
    duration: Duration,
    kind: &'static str,
    via: &'static str,
    success: bool,
) {
    turso_query_duration_histogram().record(
        duration.as_secs_f64() * 1_000.0,
        &[
            KeyValue::new("kind", kind),
            KeyValue::new("via", via),
            KeyValue::new("success", success),
        ],
    );
}

/// Connection wrapper that records Turso query/execute latency metrics.
pub(crate) struct InstrumentedConnection {
    inner: libsql::Connection,
}

impl InstrumentedConnection {
    pub(super) fn new(inner: libsql::Connection) -> Self {
        Self { inner }
    }

    pub(crate) async fn query(
        &self,
        sql: &str,
        params: impl libsql::params::IntoParams,
    ) -> Result<libsql::Rows, libsql::Error> {
        let start = Instant::now();
        let result = self.inner.query(sql, params).await;
        record_turso_query_duration(start.elapsed(), "query", "connection", result.is_ok());
        result
    }

    pub(crate) async fn execute(
        &self,
        sql: &str,
        params: impl libsql::params::IntoParams,
    ) -> Result<u64, libsql::Error> {
        let start = Instant::now();
        let result = self.inner.execute(sql, params).await;
        record_turso_query_duration(start.elapsed(), "execute", "connection", result.is_ok());
        result
    }
}

impl Deref for InstrumentedConnection {
    type Target = libsql::Connection;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
