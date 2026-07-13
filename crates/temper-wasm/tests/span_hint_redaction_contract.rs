//! ARN-243 wiring contract: every host HTTP-call site that parses guest span
//! hints must filter LLM content before applying them, or host-captured
//! prompts/completions would bypass the per-tenant export gate. Unit tests on
//! `redact_llm_content_hints` cannot see whether each call site actually calls
//! it; this source-level contract does. See ADR-0166.

const HOST_TRAIT_SOURCE: &str = include_str!("../src/host_trait.rs");

#[test]
fn every_span_hint_split_is_followed_by_redaction() {
    let src = HOST_TRAIT_SOURCE;
    let split_marker = "= split_span_hint_headers(";
    let mut sites = 0;
    let mut from = 0;
    while let Some(rel) = src[from..].find(split_marker) {
        let at = from + rel;
        let window_end = (at + 400).min(src.len());
        let window = &src[at..window_end];
        assert!(
            window.contains("redact_llm_content_hints(&mut span_hints"),
            "split_span_hint_headers call at byte {at} is not immediately followed by \
             redact_llm_content_hints; host-captured LLM content would bypass the \
             per-tenant export gate (ARN-243)"
        );
        sites += 1;
        from = at + split_marker.len();
    }
    assert!(
        sites >= 4,
        "expected at least 4 span-hint split sites to guard, found {sites}"
    );
}
