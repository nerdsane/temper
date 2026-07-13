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
        assert!(
            window.contains(redact_marker),
            "split_span_hint_headers call at byte {at} is not followed by \
             redact_llm_content_hints before the next split site; host-captured LLM \
             content would bypass the per-tenant export gate (ARN-243)"
        );
    }
}
