//! ARN-243 wiring contract for the guest manual-span API.
//!
//! The unit tests for this channel call `allowed_attributes` directly, so they
//! stay green if a call site stops calling it — the same "delete the call"
//! hole adversarial review found on the wide-event path. Every entry point that
//! accepts guest attributes must route them through the filter before they reach
//! a span or the manual-export snapshot. See ADR-0166.

const GUEST_SPANS_SOURCE: &str = include_str!("../src/engine/guest_spans.rs");

/// Blank out `//` line comments so a commented-out call cannot satisfy the
/// contract. Byte offsets are preserved (comment bodies become spaces) so
/// ordering assertions still compare positions in the original file. String
/// literals are tracked so a `"http://…"` is not mistaken for a comment.
///
/// A heuristic, not a lexer: char literals and raw strings are not modelled.
/// Every assertion here asserts *presence*, so a mis-scan fails loudly.
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

/// Every function that takes a guest payload and puts its attributes on a span
/// must filter them first, and must do it before any consumer reads them.
#[test]
fn every_guest_span_entry_point_filters_attributes_before_use() {
    let src = &strip_line_comments(GUEST_SPANS_SOURCE);
    let entry_points = [
        "pub(crate) fn start_span(",
        "pub(crate) fn add_span_event(",
        "pub(crate) fn set_span_attributes(",
        "pub(crate) fn end_span(",
    ];

    for entry in entry_points {
        let start = src
            .find(entry)
            .unwrap_or_else(|| panic!("guest span entry point `{entry}` not found"));
        let end = src[start + entry.len()..]
            .find("\n    pub(crate) fn ")
            .map_or(src.len(), |rel| start + entry.len() + rel);
        let body = &src[start..end];

        let filter_at = body
            .find("allowed_attributes(&payload.attributes")
            .unwrap_or_else(|| {
                panic!(
                    "`{entry}` must pass guest attributes through `allowed_attributes` \
                 before recording them; without it a module publishes prompts and \
                 completions on its own span for a non-opted-in tenant (ARN-243)"
                )
            });
        assert!(
            body[filter_at..].contains("export_llm_content"),
            "`{entry}` must filter with the per-tenant policy, not a constant"
        );

        // Nothing may consume the raw payload attributes after that point.
        for consumer in ["apply_attributes(", "manual_span_attributes("] {
            let mut scan = 0;
            while let Some(rel) = body[scan..].find(consumer) {
                let at = scan + rel;
                let args_end = body[at..]
                    .find(");")
                    .map_or(body.len(), |rel| (at + rel).min(body.len()));
                let args = &body[at..args_end];
                assert!(
                    !args.contains("payload.attributes"),
                    "`{entry}` passes the raw guest attributes to `{consumer}`; it must \
                     pass the filtered map (ARN-243)"
                );
                assert!(
                    filter_at < at,
                    "`{entry}` calls `{consumer}` before filtering the guest attributes"
                );
                scan = at + consumer.len();
            }
        }
    }
}
