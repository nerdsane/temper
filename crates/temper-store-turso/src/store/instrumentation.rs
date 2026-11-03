//! Turso query latency instrumentation — histogram recording and instrumented
//! connection wrapper.
//!
//! Exported for use by all `store` sub-modules via `super::instrumentation`.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::Histogram;

use super::write_gate::GatePermit;

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

/// A connection for reading, recording each query's latency. It cannot
/// write: a write needs a [`WriteConnection`], which holds the write gate's
/// permit.
pub(crate) struct InstrumentedConnection {
    inner: crate::driver::Connection,
}

impl InstrumentedConnection {
    pub(super) fn new(inner: crate::driver::Connection) -> Self {
        Self { inner }
    }

    pub(crate) async fn query(
        &self,
        sql: &str,
        params: impl turso_serverless::params::IntoParams + Send,
    ) -> Result<crate::driver::Rows, crate::driver::DriverError> {
        let start = Instant::now();
        let result = self.inner.query(sql, params).await;
        record_turso_query_duration(start.elapsed(), "query", "connection", result.is_ok());
        result
    }
}

/// A connection that may write. It holds the write gate's permit until it is
/// dropped, so writes to the database take turns at the gate and never wait
/// for one another at the database's write lock (`super::write_gate`).
pub(crate) struct WriteConnection {
    // Declared before the permit, so it is closed before the permit is free.
    conn: InstrumentedConnection,
    _permit: GatePermit,
}

impl WriteConnection {
    pub(super) fn new(conn: InstrumentedConnection, permit: GatePermit) -> Self {
        Self {
            conn,
            _permit: permit,
        }
    }

    pub(crate) async fn execute(
        &self,
        sql: &str,
        params: impl turso_serverless::params::IntoParams + Send,
    ) -> Result<u64, crate::driver::DriverError> {
        let start = Instant::now();
        let result = self.conn.inner.execute(sql, params).await;
        record_turso_query_duration(start.elapsed(), "execute", "connection", result.is_ok());
        result
    }

    pub(crate) fn begin_immediate(
        &self,
    ) -> impl std::future::Future<
        Output = Result<crate::driver::Transaction<'_>, crate::driver::DriverError>,
    > + Send {
        self.conn.inner.begin_immediate()
    }
}

/// A connection that may write may also read.
impl std::ops::Deref for WriteConnection {
    type Target = InstrumentedConnection;

    fn deref(&self) -> &Self::Target {
        &self.conn
    }
}
