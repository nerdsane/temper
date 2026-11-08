//! Choosing the trace sampler with `OTEL_TRACES_SAMPLER`.

use crate::*;

/// With `always_on`, a request that arrives marked "not sampled" still
/// produces a span, and the span keeps the caller's trace ID.
#[test]
fn always_on_records_a_request_marked_not_sampled() {
    let run = child::run(&[("OTEL_TRACES_SAMPLER", "always_on")]);
    let spans = exported_spans(&run);
    assert!(
        spans.contains(&(
            "test.unsampled_caller".to_string(),
            child::UNSAMPLED_CALLER_TRACE_ID.to_string()
        )),
        "no span with the caller's trace ID in {spans:?}"
    );
    assert_eq!(span_names(&run), EVERY_SPAN);
}

/// With the sampler unset, a request marked "not sampled" produces no span.
#[test]
fn default_sampler_follows_a_caller_that_did_not_sample() {
    let run = child::run(&[]);
    assert_eq!(span_names(&run), SPANS_FOLLOWING_THE_CALLER);
}

#[test]
fn traceidratio_zero_exports_no_spans() {
    let run = child::run(&[
        ("OTEL_TRACES_SAMPLER", "traceidratio"),
        ("OTEL_TRACES_SAMPLER_ARG", "0"),
    ]);
    assert_eq!(span_names(&run), Vec::<String>::new());
}

#[test]
fn traceidratio_one_exports_every_span() {
    let run = child::run(&[
        ("OTEL_TRACES_SAMPLER", "traceidratio"),
        ("OTEL_TRACES_SAMPLER_ARG", "1"),
    ]);
    assert_eq!(span_names(&run), EVERY_SPAN);
}

/// Every supported sampler exports what it should, and under every one of
/// them the name-based filter drops what it drops by default: the two names
/// it drops outright. A reduced-rate name inside a caller's trace is kept
/// even on a trace ID outside the rate, so a kept trace has no hole.
#[test]
fn every_sampler_keeps_the_name_based_filter() {
    let cases: [(&str, &str, &[&str]); 9] = [
        ("always_on", "", &EVERY_SPAN),
        ("always_off", "", &[]),
        ("parentbased_always_on", "", &SPANS_FOLLOWING_THE_CALLER),
        ("parentbased_always_off", "", &SPANS_WITH_A_SAMPLED_CALLER),
        ("traceidratio", "1", &EVERY_SPAN),
        ("traceidratio", "0", &[]),
        ("traceidratio", "", &EVERY_SPAN),
        ("parentbased_traceidratio", "1", &SPANS_FOLLOWING_THE_CALLER),
        (
            "parentbased_traceidratio",
            "0",
            &SPANS_WITH_A_SAMPLED_CALLER,
        ),
    ];
    for (sampler, arg, expected) in cases {
        let run = child::run(&[
            ("OTEL_TRACES_SAMPLER", sampler),
            ("OTEL_TRACES_SAMPLER_ARG", arg),
        ]);
        let case = format!("OTEL_TRACES_SAMPLER={sampler} OTEL_TRACES_SAMPLER_ARG={arg}");
        assert_eq!(span_names(&run), expected, "{case}");
        for (name, trace_id) in exported_spans(&run) {
            assert_ne!(name, "clock_time_get", "{case}");
            assert_ne!(name, "turso.configured_connection", "{case}");
            if name == "wasm:workspace_fs.read" {
                assert_eq!(trace_id, child::REDUCED_RATE_DROPPED_TRACE_ID, "{case}");
            }
        }
        assert_eq!(warnings(&run), Vec::<String>::new(), "{case}");
    }
}
