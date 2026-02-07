//! Background sentinel that periodically analyzes trajectories.

use std::time::Duration;

use temper_evolution::{
    ObservationClass, ObservationRecord, RecordHeader, RecordStore, RecordType,
};
use temper_observe::clickhouse::ClickHouseStore;
use temper_observe::ObservabilityStore;

/// Run the sentinel background loop, querying ClickHouse at the given interval.
pub async fn run_sentinel(clickhouse_url: &str, interval_secs: u64) {
    let store = ClickHouseStore::new(clickhouse_url);
    let records = RecordStore::new();
    let mut tick = tokio::time::interval(Duration::from_secs(interval_secs));

    tracing::info!(interval_secs, "Sentinel started");

    loop {
        tick.tick().await;

        // Query ClickHouse for trajectory patterns
        let total = store
            .query_spans(
                "SELECT count(*) as cnt FROM spans WHERE service = 'temper-agent'",
                &[],
            )
            .await
            .ok()
            .and_then(|rs| rs.rows.first()?.get("cnt")?.as_u64())
            .unwrap_or(0);

        if total == 0 {
            continue;
        }

        // Check for error rate
        let errors = store
            .query_spans(
                "SELECT count(*) as cnt FROM spans WHERE service = 'temper' AND status = 'error'",
                &[],
            )
            .await
            .ok()
            .and_then(|rs| rs.rows.first()?.get("cnt")?.as_u64())
            .unwrap_or(0);

        let error_rate = errors as f64 / total.max(1) as f64;

        if error_rate > 0.1 {
            let obs = ObservationRecord {
                header: RecordHeader::new(RecordType::Observation, "sentinel"),
                source: "sentinel:error_rate".into(),
                classification: ObservationClass::ErrorRate,
                evidence_query: "SELECT count(*) FROM spans WHERE status = 'error'".into(),
                threshold_field: Some("error_rate".into()),
                threshold_value: Some(0.1),
                observed_value: Some(error_rate),
                context: serde_json::json!({"total": total, "errors": errors}),
            };
            tracing::warn!(
                error_rate = format!("{:.1}%", error_rate * 100.0),
                record = %obs.header.id,
                "Sentinel: error rate exceeds threshold"
            );
            records.insert_observation(obs);
        }

        tracing::debug!(total, errors, "Sentinel tick complete");
    }
}
