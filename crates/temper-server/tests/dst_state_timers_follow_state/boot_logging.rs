//! Boot diagnostics scale with types, and the summary survives WARN-only OTLP.
use std::sync::{Arc, Mutex};
use std::time::Duration;

use opentelemetry_otlp::{Protocol, WithExportConfig};
use opentelemetry_sdk::logs::SdkLoggerProvider;
use serde_json::Value;
use temper_runtime::persistence::EventStore;
use temper_runtime::scheduler::{DeterministicIdGen, install_sim_context};
use temper_runtime::tenant::TenantId;
use temper_store_sim::SimEventStore;
use tracing_subscriber::prelude::*;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};

use super::world::SimTimerRows;
use super::{JOB_V1, Job, Process, ProcessClock, World, act, job_untimed};

#[derive(Clone, Default)]
struct Lines(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for Lines {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Lines {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[test]
fn boot_logs_scale_with_types_and_export_summary() {
    // The collector's clock is independent of the simulated server clock.
    let collector_runtime = tokio::runtime::Runtime::new().unwrap(); // determinism-ok: test OTLP HTTP collector
    let collector = collector_runtime.block_on(MockServer::start());
    collector_runtime.block_on(
        Mock::given(path("/v1/logs"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&collector),
    );
    let exporter = opentelemetry_otlp::LogExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpJson)
        .with_endpoint(format!("{}/v1/logs", collector.uri()))
        .build()
        .unwrap();
    let provider = SdkLoggerProvider::builder()
        .with_batch_exporter(exporter)
        .build();
    let lines = Lines::default();
    let debug_lines = Lines::default();
    // With one registered dispatcher, tracing-core registers a callsite using
    // the emitting thread's default. Keep a second dispatcher alive so another
    // simulation cannot register our boot diagnostics as globally disabled.
    let _unobserved = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
    let capture = || {
        tracing::Dispatch::new(
            tracing_subscriber::registry()
                .with(
                    tracing_subscriber::fmt::layer()
                        .json()
                        .with_writer(debug_lines.clone())
                        .with_filter(tracing_subscriber::filter::LevelFilter::DEBUG),
                )
                .with(
                    tracing_subscriber::fmt::layer()
                        .json()
                        .with_writer(lines.clone())
                        .with_filter(tracing_subscriber::filter::LevelFilter::INFO),
                )
                .with(
                    opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(
                        &provider,
                    )
                    .with_filter(tracing_subscriber::filter::LevelFilter::WARN),
                ),
        )
    };
    let mut line_counts = Vec::new();
    for records in [4, 256] {
        let clock = Arc::new(ProcessClock::new());
        let _sim = install_sim_context(clock.clone(), Arc::new(DeterministicIdGen::new(71)));
        let world = World {
            events: SimEventStore::no_faults(71),
            rows: Arc::new(SimTimerRows::new(71)),
            clock,
        };
        let first = Process::start(&world, JOB_V1, &job_untimed(), "logging-first");
        first.runtime.block_on(async {
            for i in 0..records {
                let id = format!("job-{i}");
                first
                    .state
                    .get_or_create_tenant_entity(
                        &TenantId::new("alpha"),
                        "Job",
                        &id,
                        serde_json::json!({}),
                    )
                    .await
                    .unwrap();
                assert!(
                    act(
                        &first.state,
                        &Job {
                            tenant: TenantId::new("alpha"),
                            id: id.clone(),
                        },
                        "Start"
                    )
                    .await
                );
                // Half snapshot-only, half journal-only: both replay paths
                // must remain quiet during the walk.
                if i % 2 == 0 {
                    let state = first
                        .state
                        .get_tenant_entity_state(&TenantId::new("alpha"), "Job", &id)
                        .await
                        .unwrap()
                        .state;
                    world
                        .events
                        .save_snapshot(
                            &format!("alpha:Job:{id}"),
                            state.sequence_nr,
                            &serde_json::to_vec(&state).unwrap(),
                        )
                        .await
                        .unwrap();
                }
            }
            super::settle().await;
        });
        first.kill(&world, Duration::ZERO);
        lines.0.lock().unwrap().clear();
        debug_lines.0.lock().unwrap().clear();
        // Register after the unobserved setup boot so per-layer callsite
        // interests are rebuilt for the subscriber used by the measured boot.
        let dispatch = capture();
        let _logs = tracing::dispatcher::set_default(&dispatch);
        let second = Process::start(&world, JOB_V1, &job_untimed(), "logging-second");
        let captured: Vec<Value> = lines
            .0
            .lock()
            .unwrap()
            .split(|b| *b == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect();
        assert!(
            !captured
                .iter()
                .any(|line| line["target"] == "temper_server::entity_actor::actor"),
            "walk emitted per-record replay lines: {captured:?}"
        );
        assert_eq!(
            captured
                .iter()
                .filter(
                    |line| line["fields"]["message"] == "state timers follow the records' states"
                )
                .count(),
            2,
            "captured boot logs: {captured:?}"
        );
        assert_eq!(
            captured
                .iter()
                .filter(
                    |line| line["fields"]["message"] == "state timers after boot"
                        && line["level"] == "WARN"
                )
                .count(),
            1
        );
        let summary = captured
            .iter()
            .find(|line| line["fields"]["message"] == "state timers after boot")
            .unwrap();
        assert_eq!(summary["fields"]["rearmed"], records);
        assert_eq!(summary["fields"]["dropped"], 0);
        assert_eq!(summary["fields"]["waiting"], 0);
        assert_eq!(summary["fields"]["scheduled"], 0);
        let debug_replays = String::from_utf8(debug_lines.0.lock().unwrap().clone()).unwrap();
        assert_eq!(
            debug_replays
                .lines()
                .filter(|line| {
                    let line: Value = serde_json::from_str(line).unwrap();
                    line["target"] == "temper_server::entity_actor::actor"
                        && line["level"] == "DEBUG"
                        && matches!(
                            line["fields"]["message"].as_str(),
                            Some(
                                "loaded snapshot before replay"
                                    | "state restored from snapshot (no delta events)"
                                    | "state rebuilt from event journal via TransitionTable"
                            )
                        )
                })
                .count(),
            records * 3 / 2
        );
        line_counts.push(captured.len());
        lines.0.lock().unwrap().clear();
        second
            .runtime
            .block_on(
                second
                    .state
                    .get_tenant_entity_state(&TenantId::new("alpha"), "Job", "job-0"),
            )
            .unwrap();
        let actor_lines = String::from_utf8(lines.0.lock().unwrap().clone()).unwrap();
        for message in [
            "loaded snapshot before replay",
            "state restored from snapshot (no delta events)",
        ] {
            assert!(
                actor_lines
                    .lines()
                    .any(|line| line.contains(message) && line.contains("\"level\":\"INFO\"")),
                "actor lost info diagnostic: {message}"
            );
        }
        second.kill(&world, Duration::ZERO);
    }
    assert_eq!(
        line_counts[0], line_counts[1],
        "boot log volume grew with records"
    );
    provider.force_flush().unwrap();
    let requests = collector_runtime
        .block_on(collector.received_requests())
        .unwrap();
    let mut summaries = Vec::new();
    for request in requests {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        for resource in body["resourceLogs"].as_array().unwrap() {
            for scope in resource["scopeLogs"].as_array().unwrap() {
                for record in scope["logRecords"].as_array().unwrap() {
                    if record["body"]["stringValue"] == "state timers after boot" {
                        summaries.push(record.clone());
                    }
                }
            }
        }
    }
    assert_eq!(summaries.len(), 2, "summary missing from OTLP HTTP output");
    for summary in summaries {
        assert_eq!(summary["severityText"], "WARN");
        let attributes = summary["attributes"].as_array().unwrap();
        for key in ["rearmed", "dropped", "waiting", "scheduled", "by_type"] {
            assert!(
                attributes.iter().any(|attribute| attribute["key"] == key),
                "missing {key}: {summary}"
            );
        }
    }
    provider.shutdown().unwrap();
}
