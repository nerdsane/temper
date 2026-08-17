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
