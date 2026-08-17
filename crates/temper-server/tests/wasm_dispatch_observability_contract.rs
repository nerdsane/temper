//! Observability contract tests for WASM dispatch attribution.

const WASM_DISPATCH_SOURCE: &str = include_str!("../src/state/dispatch/wasm.rs");

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
fn wasm_dispatch_emits_integration_envelope_phase_spans() {
    let required_phases = [
        "dispatch.wasm.phase.module_cache",
        "dispatch.wasm.phase.replay_input_injection",
        "dispatch.wasm.phase.invocation_context_build",
        "dispatch.wasm.phase.blob_ref_hydration",
        "dispatch.wasm.phase.authz_secret_resolution",
        "dispatch.wasm.phase.host_chain_build",
        "dispatch.wasm.phase.integration_observe_start",
        "dispatch.wasm.phase.engine_invoke_and_handle",
        "dispatch.wasm.phase.engine_invoke",
        "dispatch.wasm.phase.result_observe_complete",
        "dispatch.wasm.phase.record_invocation",
        "dispatch.wasm.phase.dispatch_callback",
        "dispatch.wasm.phase.llmobs_submit",
    ];

    for phase in required_phases {
        assert!(
            WASM_DISPATCH_SOURCE.contains(phase),
            "missing required WASM dispatch observability phase span: {phase}"
        );
    }
}

/// ARN-243 wiring contract: per-tenant LLM content redaction must run before
/// every telemetry sink reads the callback params. A refactor that moved the
/// strip after any sink would leak prompts/completions for non-opted-in
/// tenants — this guards the ordering that unit tests on the helper cannot.
/// See ADR-0166.
#[test]
fn llm_content_redaction_precedes_every_dispatch_sink() {
    // Production code only: the file's own `#[cfg(test)]` module calls the helper
    // and names these sinks, which would otherwise both inflate the call count and
    // offer false ordering evidence.
    let prod = WASM_DISPATCH_SOURCE
        .split_once("#[cfg(test)]\nmod ")
        .map_or(WASM_DISPATCH_SOURCE, |(prod, _)| prod);
    let src = &strip_line_comments(prod);

    // Anchor on the *call*, not the comment above it: a comment can be reworded
    // or deleted while the guard still runs, and — worse — can survive after the
    // call it describes is removed, leaving this test green over a leak.
    let calls = src.matches("redact_llm_content_params(").count()
        - src.matches("fn redact_llm_content_params(").count();
    assert_eq!(
        calls, 1,
        "exactly one redaction call site is expected in dispatch; a second would \
         need its own ordering proof rather than riding on this one"
    );
    let strip = src
        .match_indices("redact_llm_content_params(")
        .map(|(at, _)| at)
        .find(|at| !src[..*at].ends_with("fn "))
        .expect("dispatch must redact LLM content before recording telemetry");

    // Every sink below reads content from `result.callback_params`; *every*
    // occurrence of each must come after the strip so it observes the redacted
    // map. Iterating all occurrences (not just the first) catches a future
    // dispatch branch that reads the params before the strip runs.
    let sinks = [
        "let callback_params = &result.callback_params;",
        "llm_call_wide_event(",
        "submit_llmobs_llm_span(",
        "submit_llmobs_tool_spans(",
    ];
    for sink in sinks {
        let mut from = 0;
        let mut found = false;
        while let Some(rel) = src[from..].find(sink) {
            let at = from + rel;
            found = true;
            assert!(
                strip < at,
                "LLM content redaction (byte {strip}) must precede every read of sink \
                 `{sink}`; found an occurrence at byte {at} before the strip"
            );
            from = at + sink.len();
        }
        assert!(found, "expected dispatch telemetry sink `{sink}`");
    }
}

/// Companion to the ordering test: pin the *argument*, not just the call. A site
/// that redacts with a hardcoded `true`, or a host built with
/// `.with_llm_content_export(true)`, satisfies every ordering and unit test while
/// exporting content for every tenant.
#[test]
fn dispatch_redaction_and_host_wiring_use_the_per_tenant_policy() {
    let prod = WASM_DISPATCH_SOURCE
        .split_once("#[cfg(test)]\nmod ")
        .map_or(WASM_DISPATCH_SOURCE, |(prod, _)| prod);
    let src = &strip_line_comments(prod);

    let redact_at = src
        .match_indices("redact_llm_content_params(")
        .map(|(at, _)| at)
        .find(|at| !src[..*at].ends_with("fn "))
        .expect("dispatch must redact LLM content");
    let call = &src[redact_at..src.len().min(redact_at + 220)];
    assert!(
        call.contains("self.export_llm_content("),
        "the dispatch redaction must be driven by the per-tenant policy, not a \
         constant; found: {call:?}"
    );

    // Every host handed to the engine must take its export flag from the policy.
    // Checked per site rather than by counting markers, so a multi-line
    // `.with_llm_content_export(\n    true,\n)` cannot pass as policy-driven.
    let marker = ".with_llm_content_export(";
    let mut sites = 0;
    for (at, _) in src.match_indices(marker) {
        sites += 1;
        let arg_start = at + marker.len();
        let arg = &src[arg_start..src.len().min(arg_start + 120)];
        let arg = arg.split_once(')').map_or(arg, |(head, _)| head);
        assert!(
            arg.contains("export_llm_content("),
            "`.with_llm_content_export` at byte {at} must be passed the per-tenant \
             policy, not a constant; found argument {arg:?}"
        );
    }
    assert!(
        sites >= 3,
        "expected the known host construction sites, found {sites}"
    );
}

#[test]
fn comment_stripper_ignores_slashes_inside_string_literals() {
    let src = "let url = \"http://example.com/a\"; // real comment\nlet keep = 1;\n";
    let stripped = strip_line_comments(src);
    assert_eq!(stripped.len(), src.len(), "offsets must be preserved");
    assert!(
        stripped.contains("http://example.com/a"),
        "a URL in a string literal must survive: {stripped:?}"
    );
    assert!(
        !stripped.contains("real comment"),
        "the comment must be blanked"
    );
    assert!(stripped.contains("let keep = 1;"));
}
