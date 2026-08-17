//! ARN-243 wiring contract: every host HTTP-call site that parses guest span
//! hints must filter LLM content before applying them, or host-captured
//! prompts/completions would bypass the per-tenant export gate. Unit tests on
//! `redact_llm_content_hints` cannot see whether each call site actually calls
//! it; this source-level contract does. See ADR-0166.

const HOST_TRAIT_SOURCE: &str = include_str!("../src/host_trait.rs");

/// Blank out `//` line comments so a commented-out call cannot satisfy a source
/// contract. Byte offsets are preserved (comment bodies become spaces) so every
/// ordering assertion still compares positions in the original file. String
/// literals are tracked, so a `"http://..."` in code is not mistaken for a
/// comment and does not blank the rest of its line.
///
/// This is a heuristic, not a lexer: char literals (`'/'`) and raw strings
/// (`r#"..."#`) are not modelled. Both scanned files are free of them today, and
/// every assertion built on this asserts *presence*, so a mis-scan produces a
/// loud failure rather than a silent pass.
fn strip_line_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    for line in src.split_inclusive('\n') {
        let bytes = line.as_bytes();
        let mut in_string = false;
        let mut escaped = false;
        let mut comment_at = None;
        for (i, &b) in bytes.iter().enumerate() {
            if escaped {
                escaped = false;
                continue;
            }
            match b {
                b'\\' if in_string => escaped = true,
                b'"' => in_string = !in_string,
                b'/' if !in_string && bytes.get(i + 1) == Some(&b'/') => {
                    comment_at = Some(i);
                    break;
                }
                _ => {}
            }
        }
        match comment_at {
            Some(at) => {
                out.push_str(&line[..at]);
                for ch in line[at..].chars() {
                    out.push(if ch == '\n' { '\n' } else { ' ' });
                }
            }
            None => out.push_str(line),
        }
    }
    out
}

#[test]
fn every_span_hint_split_is_followed_by_redaction() {
    let src = &strip_line_comments(HOST_TRAIT_SOURCE);
    let split_marker = "= split_span_hint_headers(";
    let redact_marker = "redact_llm_content_hints(&mut span_hints";

    // Collect every split-site offset first, then require the redact call to
    // appear in the interstitial code before the next split site (or EOF). The
    // window is exactly the code between two sites, so the guard holds no matter
    // how much code is inserted between the split and its redaction — no magic
    // fixed-size window that could silently exclude a displaced redact call.
    let mut sites = Vec::new();
    let mut from = 0;
    while let Some(rel) = src[from..].find(split_marker) {
        let at = from + rel;
        sites.push(at);
        from = at + split_marker.len();
    }
    assert!(
        sites.len() >= 4,
        "expected at least 4 span-hint split sites to guard, found {}",
        sites.len()
    );
    for (i, &at) in sites.iter().enumerate() {
        let end = sites.get(i + 1).copied().unwrap_or(src.len());
        let window = &src[at..end];
        let redact_at = window.find(redact_marker).unwrap_or_else(|| {
            panic!(
                "split_span_hint_headers call at byte {at} is not followed by \
                 redact_llm_content_hints before the next split site; host-captured LLM \
                 content would bypass the per-tenant export gate (ARN-243)"
            )
        });

        // Presence is not enough: the redaction has to run *before* the hints are
        // applied. Moving the redact call below `apply_span_hints` leaves the
        // markers all present while exporting every content attribute, so pin the
        // order rather than the existence.
        for apply_marker in ["apply_span_hints(", "apply_response_captures("] {
            let mut scan = 0;
            while let Some(rel) = window[scan..].find(apply_marker) {
                let apply_at = scan + rel;
                assert!(
                    redact_at < apply_at,
                    "in the span-hint block at byte {at}, `{apply_marker}` (offset \
                     {apply_at}) runs before redact_llm_content_hints (offset \
                     {redact_at}); the hints would be applied unredacted (ARN-243)"
                );
                scan = apply_at + apply_marker.len();
            }
        }
    }
}

/// The gate is only as good as the value handed to it. A call site that passes a
/// literal `true` — or any expression that is not the host's per-tenant decision
/// — exports content for every tenant while every ordering and unit test stays
/// green. This is the mutation an ordering contract cannot see.
#[test]
fn span_hint_redaction_uses_the_per_tenant_policy_not_a_literal() {
    let src = &strip_line_comments(HOST_TRAIT_SOURCE);
    let expected = "redact_llm_content_hints(&mut span_hints, self.export_llm_content);";
    let calls = src
        .matches("redact_llm_content_hints(&mut span_hints")
        .count();
    let policy_calls = src.matches(expected).count();
    assert_eq!(
        calls,
        policy_calls,
        "every span-hint redaction must pass `self.export_llm_content`; {} of {calls} \
         call sites pass something else (a hardcoded `true` would leak every tenant)",
        calls - policy_calls
    );
    assert!(
        policy_calls >= 4,
        "expected at least 4 guarded sites, found {policy_calls}"
    );
}

/// The guest metric channel (`host_emit_metric`) has two sinks that read the
/// guest's tags — a span event and the OTel meter. Unit tests on the helper
/// cannot see whether `emit_metric` calls it, nor whether it calls it early
/// enough; this pins both.
#[test]
fn guest_metric_tags_are_redacted_before_either_sink() {
    let src = &strip_line_comments(HOST_TRAIT_SOURCE);
    let start = src
        .find("fn emit_metric(&self, metric_json: &str)")
        .expect("ProductionWasmHost must implement emit_metric");
    // Scope to this function: the trait's default impl and SimWasmHost's also
    // define `emit_metric`, and only the production one carries guest tags.
    let end = src[start..]
        .find("\n    fn ")
        .map_or(src.len(), |rel| start + rel);
    let body = &src[start..end];

    let redact_at = body
        .find("redact_guest_string_tags(&mut payload.tags")
        .expect(
            "emit_metric must redact guest tags: they are guest-named and \
             guest-valued strings that reach OTel metrics and a span event (ARN-243)",
        );
    for sink in [
        "record_guest_metric_span_event(",
        "payload\n            .tags",
    ] {
        if let Some(sink_at) = body.find(sink) {
            assert!(
                redact_at < sink_at,
                "guest metric tags must be redacted before `{sink}` reads them; \
                 redact at {redact_at}, sink at {sink_at}"
            );
        }
    }
    assert!(
        body.contains("self.export_llm_content"),
        "the metric redaction must be driven by the per-tenant policy, not a constant"
    );
}

/// `build_guest_wide_event` is the other guest-authored telemetry record. Its
/// unit tests call the helper directly, so deleting the *call* here left them all
/// green — this pins the wiring, and that the guest half is judged before any
/// host-derived field is merged in.
#[test]
fn guest_wide_event_fields_are_redacted_at_the_call_site() {
    let src = &strip_line_comments(HOST_TRAIT_SOURCE);
    let start = src
        .find("fn build_guest_wide_event(&self, event_json: &str)")
        .expect("ProductionWasmHost must build guest wide events");
    let end = src[start..]
        .find("\n    fn ")
        .map_or(src.len(), |rel| start + rel);
    let body = &src[start..end];

    let redact_at = body
        .find("redact_guest_wide_event_fields(&mut tags, &mut attributes")
        .expect(
            "build_guest_wide_event must redact the guest-supplied tags and \
             attributes: a wide event carries guest-chosen names and values \
             straight to the backend (ARN-243)",
        );
    assert!(
        body[redact_at..].contains("self.export_llm_content"),
        "the wide-event redaction must be driven by the per-tenant policy, not a constant"
    );
    // The host merges its own fields (tenant, entity_type, trigger_action) in
    // after this point; judging them would be wrong, so the redaction has to come
    // first — which is also what keeps the guest half from being judged twice.
    if let Some(merge_at) = body.find(".entry(\"tenant\".into())") {
        assert!(
            redact_at < merge_at,
            "guest fields must be redacted before host-derived fields are merged in"
        );
    }
}
