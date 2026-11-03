//! Runs the telemetry setup in a child process and records what it exports.
//!
//! The setup installs process-wide state (the global tracer and meter
//! providers and the `tracing` subscriber) and reads its settings from the
//! environment, so each scenario gets a process of its own: this test binary,
//! started again with only [`entry`] selected.

use std::process::Command;
use std::time::Duration;

use opentelemetry::trace::{SpanContext, SpanId, TraceContextExt, TraceFlags, TraceId, TraceState};
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::listener::{Listener, Received};
use crate::wire::{self, Attributes, LogRecord, Metric, Span};

const SCENARIO_ENV: &str = "OTEL_EXPORT_TEST_SCENARIO";
const WORKLOAD: &str = "workload";
const LOG_PROBE_THEN_WORKLOAD: &str = "log-probe-then-workload";
const LOG_AFTER_SHUTDOWN: &str = "log-after-shutdown";
const WORKLOAD_THEN_WAIT: &str = "workload-then-wait";

/// How long the child keeps running after the workload in
/// [`run_with_resets`]. The batch processors send every second, and a batch
/// sent twice more after it failed has arrived 0.75 s after that.
const WAIT_AFTER_WORKLOAD: Duration = Duration::from_secs(3);

/// The line the child logs once its telemetry has shut down.
pub const AFTER_SHUTDOWN_LINE: &str = "test log after the telemetry shut down";

/// Service name `temper serve` passes to the telemetry setup.
pub const BUILT_IN_SERVICE_NAME: &str = "temper-platform";

/// Trace ID of the caller whose request arrives marked "not sampled".
pub const UNSAMPLED_CALLER_TRACE_ID: &str = "0af7651916cd43dd8448eb211c80319c";
/// Trace ID of the caller whose request arrives marked "sampled".
pub const SAMPLED_CALLER_TRACE_ID: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
/// The reduced-rate rules keep a trace when the low 64 bits of its ID, modulo
/// 100, are below the rule's rate. 100 % 100 = 0 is kept at every rate above 0.
pub const REDUCED_RATE_KEPT_TRACE_ID: &str = "00000000000000000000000000000064";
/// 99 % 100 = 99 is dropped at every rate below 100.
pub const REDUCED_RATE_DROPPED_TRACE_ID: &str = "00000000000000000000000000000063";

const CALLER_SPAN_ID: &str = "b7ad6b7169203331";

/// What one child process exported and printed.
pub struct Run {
    pub endpoint: String,
    pub received: Vec<Received>,
    /// The `tracing` log lines, one JSON object per line, among the test
    /// harness's own output.
    pub stdout: String,
    /// What the setup prints before the `tracing` subscriber exists.
    pub stderr: String,
}

impl Run {
    pub fn spans(&self) -> Vec<Span> {
        self.bodies("/v1/traces").flat_map(wire::spans).collect()
    }

    pub fn logs(&self) -> Vec<LogRecord> {
        self.bodies("/v1/logs").flat_map(wire::logs).collect()
    }

    /// The metrics of the last export. Every export carries all metrics
    /// again, so the earlier ones would only repeat them.
    pub fn metrics(&self) -> Vec<Metric> {
        self.bodies("/v1/metrics")
            .last()
            .map(wire::metrics)
            .unwrap_or_default()
    }

    /// The resource each signal was exported under: one entry per signal
    /// that reached the listener. A signal sent under more than one resource
    /// is a failure.
    pub fn resources(&self) -> Vec<(&'static str, Attributes)> {
        let traces = self.spans().into_iter().map(|span| span.resource);
        let metrics = self.metrics().into_iter().map(|metric| metric.resource);
        let logs = self.logs().into_iter().map(|record| record.resource);
        let mut out = Vec::new();
        for (signal, mut resources) in [
            ("traces", traces.collect::<Vec<_>>()),
            ("metrics", metrics.collect()),
            ("logs", logs.collect()),
        ] {
            resources.dedup();
            assert!(
                resources.len() <= 1,
                "{signal} were exported under more than one resource: {resources:?}"
            );
            out.extend(resources.into_iter().map(|resource| (signal, resource)));
        }
        out
    }

    /// Number of requests the listener received on `path`.
    pub fn requests_to(&self, path: &str) -> usize {
        self.bodies(path).count()
    }

    /// The log lines the child printed, parsed, in order.
    pub fn log_lines(&self) -> Vec<serde_json::Map<String, serde_json::Value>> {
        self.stdout
            .lines()
            // The test harness may print the test's name in front of the
            // first line.
            .filter_map(|line| line.find('{').map(|start| &line[start..]))
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    /// The bodies sent to `path`, each checked to be OTLP protobuf, the only
    /// encoding [`wire`] reads.
    fn bodies<'a>(&'a self, path: &'a str) -> impl Iterator<Item = &'a [u8]> {
        self.received
            .iter()
            .filter(move |request| request.path == path)
            .map(|request| {
                assert_eq!(
                    request.header("content-type"),
                    "application/x-protobuf",
                    "export to {} is not OTLP protobuf",
                    request.path
                );
                request.body.as_slice()
            })
    }
}

/// Start the telemetry setup in a child process with `env` added to a clean
/// environment, run the workload, and collect what reached the listener.
pub fn run(env: &[(&str, &str)]) -> Run {
    run_scenario(WORKLOAD, env)
}

/// Like [`run`], but the child asks the `log` bridge whether logging is
/// enabled, and logs nothing, before it runs the workload.
pub fn run_after_log_probe(env: &[(&str, &str)]) -> Run {
    run_scenario(LOG_PROBE_THEN_WORKLOAD, env)
}

/// Like [`run`], but the child logs one more line after it has shut its
/// telemetry down, as `temper serve`'s other tasks can while it stops.
pub fn run_then_log_after_shutdown(env: &[(&str, &str)]) -> Run {
    run_scenario(LOG_AFTER_SHUTDOWN, env)
}

/// Like [`run`], but the listener closes the first `resets` connections the
/// child opens, and the child keeps running for [`WAIT_AFTER_WORKLOAD`]
/// after the workload, as a server does after it boots.
pub fn run_with_resets(resets: usize, env: &[(&str, &str)]) -> Run {
    run_scenario_against(Listener::start_resetting(resets), WORKLOAD_THEN_WAIT, env)
}

fn run_scenario(scenario: &str, env: &[(&str, &str)]) -> Run {
    run_scenario_against(Listener::start(), scenario, env)
}

fn run_scenario_against(listener: Listener, scenario: &str, env: &[(&str, &str)]) -> Run {
    let mut command = Command::new(std::env::current_exe().expect("path of the test binary"));
    command.args(["--exact", "child::entry", "--nocapture"]);
    for (name, _) in std::env::vars_os() {
        if is_telemetry_variable(&name.to_string_lossy()) {
            command.env_remove(name);
        }
    }
    command
        .env(SCENARIO_ENV, scenario)
        .env("OTLP_ENDPOINT", listener.endpoint());
    for (name, value) in env {
        command.env(name, value);
    }

    let output = command.output().expect("start the child process");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "child process for scenario {scenario} failed:\n{stdout}\n{stderr}"
    );
    Run {
        endpoint: listener.endpoint(),
        received: listener.received(),
        stdout,
        stderr,
    }
}

/// Variables the telemetry setup reads; none may leak in from the machine
/// running the tests.
fn is_telemetry_variable(name: &str) -> bool {
    ["OTEL_", "OTLP_", "DD_", "LOGFIRE_", "TEMPER_"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
        || name == "RUST_LOG"
        || name == "FOREGROUND_LOGS"
}

/// The child side. A no-op unless a parent test started this process.
#[test]
fn entry() {
    let Ok(scenario) = std::env::var(SCENARIO_ENV) else {
        return;
    };
    let guard = temper_observe::otel::init_observability(BUILT_IN_SERVICE_NAME)
        .expect("the OTEL pipeline must start when an endpoint is configured");
    if scenario == LOG_PROBE_THEN_WORKLOAD {
        probe_log_enabled();
    }
    emit_workload();
    if scenario == WORKLOAD_THEN_WAIT {
        std::thread::sleep(WAIT_AFTER_WORKLOAD);
    }
    guard.shutdown();
    if scenario == LOG_AFTER_SHUTDOWN {
        tracing::warn!("{AFTER_SHUTDOWN_LINE}");
    }
}

/// Ask the `log` bridge whether logging is enabled without logging anything:
/// once for a level and target that are enabled, once for a target the
/// default filter turns down, and once for a level that is off.
fn probe_log_enabled() {
    assert!(log::log_enabled!(log::Level::Info));
    assert!(!log::log_enabled!(target: "hyper", log::Level::Info));
    assert!(!log::log_enabled!(log::Level::Trace));
}

/// A span named `$name` whose remote parent is the given caller.
macro_rules! span_from_caller {
    ($name:literal, $trace_id:expr, $flags:expr) => {{
        let span = tracing::info_span!($name);
        span.set_parent(caller_context($trace_id, $flags));
        span.in_scope(|| {});
    }};
}

/// The same telemetry in every scenario: spans, log records and a metric.
fn emit_workload() {
    tracing::info_span!("test.request").in_scope(|| {
        tracing::info!(detail = "inside", "test log inside a span");
        tracing::info_span!("test.child").in_scope(|| {});
    });
    tracing::warn!("test log outside a span");

    span_from_caller!(
        "test.unsampled_caller",
        UNSAMPLED_CALLER_TRACE_ID,
        TraceFlags::default()
    );
    span_from_caller!(
        "test.sampled_caller",
        SAMPLED_CALLER_TRACE_ID,
        TraceFlags::SAMPLED
    );

    // Names the name-based filter drops outright.
    tracing::info_span!("clock_time_get").in_scope(|| {});
    span_from_caller!(
        "turso.configured_connection",
        SAMPLED_CALLER_TRACE_ID,
        TraceFlags::SAMPLED
    );
    // A name the name-based filter trims only when it starts its own trace. In a
    // caller's trace it is kept, even on a trace ID outside the reduced rate:
    // trimming it there would leave a hole in a trace that is kept.
    span_from_caller!(
        "wasm:workspace_fs.read",
        REDUCED_RATE_DROPPED_TRACE_ID,
        TraceFlags::SAMPLED
    );

    opentelemetry::global::meter("otel_export_test")
        .u64_counter("otel_export_test_counter")
        .build()
        .add(1, &[]);
}

fn caller_context(trace_id: &str, flags: TraceFlags) -> opentelemetry::Context {
    let caller = SpanContext::new(
        TraceId::from_hex(trace_id).expect("valid trace id"),
        SpanId::from_hex(CALLER_SPAN_ID).expect("valid span id"),
        flags,
        true,
        TraceState::default(),
    );
    opentelemetry::Context::new().with_remote_span_context(caller)
}
