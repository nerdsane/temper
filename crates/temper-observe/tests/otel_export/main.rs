//! End-to-end tests of the OTLP export settings.
//!
//! Each test starts the telemetry setup in a child process, pointed at a
//! local OTLP/HTTP listener, and asserts on what the listener received and on
//! what the child printed at startup.

mod bad_values;
mod child;
mod identity;
mod listener;
mod render;
mod retry;
mod sampling;
mod shutdown;
mod signals;
mod wire;

use child::{BUILT_IN_SERVICE_NAME, Run};
use wire::Attributes;

const BASELINE_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/otel_export/default_export.baseline.txt"
);

const SIGNAL_PATHS: [&str; 3] = ["/v1/traces", "/v1/metrics", "/v1/logs"];

/// What the workload exports when every span the name-based filter lets
/// through is sampled, whatever its caller said.
const EVERY_SPAN: [&str; 5] = [
    "test.child",
    "test.request",
    "test.sampled_caller",
    "test.unsampled_caller",
    "wasm:workspace_fs.read",
];

/// What it exports when the caller's decision is followed, as by default.
const SPANS_FOLLOWING_THE_CALLER: [&str; 4] = [
    "test.child",
    "test.request",
    "test.sampled_caller",
    "wasm:workspace_fs.read",
];

/// What it exports when only spans with a sampled caller are kept.
const SPANS_WITH_A_SAMPLED_CALLER: [&str; 2] = ["test.sampled_caller", "wasm:workspace_fs.read"];

/// The resource of each of the three signals, which must all have exported.
fn resources_of_all_signals(run: &Run) -> Vec<(&'static str, Attributes)> {
    let resources = run.resources();
    let signals: Vec<&str> = resources.iter().map(|(signal, _)| *signal).collect();
    assert_eq!(signals, ["traces", "metrics", "logs"]);
    resources
}

fn attribute<'a>(resource: &'a Attributes, key: &str) -> Option<&'a str> {
    resource.get(key).map(String::as_str)
}

/// The messages of the warnings the telemetry setup logged.
fn warnings(run: &Run) -> Vec<String> {
    run.log_lines()
        .iter()
        .filter(|line| line["level"] == "WARN" && line["target"] == "temper_observe::otel")
        .map(message)
        .collect()
}

/// The message of every log line the child printed.
fn log_messages(run: &Run) -> Vec<String> {
    run.log_lines().iter().map(message).collect()
}

fn message(line: &serde_json::Map<String, serde_json::Value>) -> String {
    line["fields"]["message"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// Name and trace ID of every exported span, sorted.
fn exported_spans(run: &Run) -> Vec<(String, String)> {
    let mut spans: Vec<(String, String)> = run
        .spans()
        .into_iter()
        .map(|span| (span.name, span.trace_id))
        .collect();
    spans.sort();
    spans
}

fn span_names(run: &Run) -> Vec<String> {
    exported_spans(run)
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

/// The startup output, the log lines and the export of `run` are those of
/// the recorded baseline.
fn assert_matches_baseline(run: &Run, what: &str) {
    let expected = std::fs::read_to_string(BASELINE_PATH).expect("read the baseline");
    let actual = render::render(run);
    assert!(
        actual == expected,
        "{what} differs from the recorded baseline\n\
         --- recorded ({BASELINE_PATH})\n{expected}\n--- actual\n{actual}"
    );
}

/// With none of the export settings present, the startup output and the
/// exported telemetry match the recorded baseline.
///
/// To record the baseline again after an intended change, run this test with
/// `UPDATE_OTEL_EXPORT_BASELINE=1` and review the diff of the baseline file.
#[test]
fn default_export_matches_recorded_baseline() {
    let run = child::run(&[]);
    for path in SIGNAL_PATHS {
        assert!(run.requests_to(path) > 0, "no export reached {path}");
    }
    if std::env::var_os("UPDATE_OTEL_EXPORT_BASELINE").is_some() {
        std::fs::write(BASELINE_PATH, render::render(&run)).expect("write the baseline");
        return;
    }
    assert_matches_baseline(&run, "the default export");
}

/// Every exported log record carries an event time. A backend that dates
/// records by their event time treats a record without one as very old.
#[test]
fn every_log_record_has_an_event_time() {
    let run = child::run(&[]);
    let records = run.logs();
    assert!(!records.is_empty(), "no log records were exported");
    for record in records {
        assert_ne!(
            record.time_unix_nano, 0,
            "log record {:?} was exported with an event time of zero",
            record.body
        );
        assert_eq!(
            record.time_unix_nano, record.observed_time_unix_nano,
            "log record {:?} has an event time that is not its observed time",
            record.body
        );
    }
}

/// Asking "is logging enabled?" through the `log` bridge, with no log line
/// after it, must not cost the next span or log line on that thread: the
/// export and the log lines are the same as without the question.
#[test]
fn log_enabled_probe_does_not_drop_the_next_span() {
    let run = child::run_after_log_probe(&[]);
    assert_eq!(span_names(&run), SPANS_FOLLOWING_THE_CALLER);
    assert_matches_baseline(&run, "the export after a log probe");
}
