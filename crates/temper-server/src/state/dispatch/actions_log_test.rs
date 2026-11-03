//! Request-body truncation for dispatch logs.

use super::{LOG_REQUEST_BODY_MAX_BYTES, truncate_request_body_for_log};

#[test]
fn short_body_is_logged_verbatim() {
    let body = r#"{"ProductId":"p-1"}"#;
    assert_eq!(truncate_request_body_for_log(body), body);
}

#[test]
fn oversized_ascii_body_is_marked_truncated() {
    let body = "a".repeat(LOG_REQUEST_BODY_MAX_BYTES + 100);
    let truncated = truncate_request_body_for_log(&body);
    assert!(truncated.ends_with("[truncated]"));
    assert_eq!(
        truncated.len(),
        LOG_REQUEST_BODY_MAX_BYTES + "[truncated]".len()
    );
}

#[test]
fn oversized_multibyte_body_does_not_panic_at_the_cap() {
    // 3-byte characters: the cap at 4096 falls mid-character (4096 % 3 != 0),
    // which a raw byte slice would panic on.
    let body = "\u{4e16}".repeat(LOG_REQUEST_BODY_MAX_BYTES);
    let truncated = truncate_request_body_for_log(&body);
    assert!(truncated.ends_with("[truncated]"));
    assert!(truncated.len() <= LOG_REQUEST_BODY_MAX_BYTES + "[truncated]".len());
}
