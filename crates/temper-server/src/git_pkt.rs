//! Git pkt-line helpers used by HttpEndpoint action-bridge responses.

use axum::body::Body;
use axum::response::Response;

pub(crate) fn sideband_channel_one(inner: Vec<u8>) -> Vec<u8> {
    let mut response = Vec::new();
    for chunk in inner.chunks(65_515) {
        let mut payload = Vec::with_capacity(1 + chunk.len());
        payload.push(0x01);
        payload.extend_from_slice(chunk);
        write_pkt_line(&mut response, &payload);
    }
    write_pkt_flush(&mut response);
    response
}

pub(crate) fn write_pkt_line(out: &mut Vec<u8>, payload: &[u8]) {
    let len = payload.len() + 4;
    out.extend_from_slice(format!("{len:04x}").as_bytes());
    out.extend_from_slice(payload);
}

pub(crate) fn write_pkt_flush(out: &mut Vec<u8>) {
    out.extend_from_slice(b"0000");
}

pub(crate) fn sanitize_git_report_text(input: &str) -> String {
    let mut out = input
        .chars()
        .map(|c| match c {
            '\r' | '\n' | '\t' => ' ',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect::<String>();
    if out.len() > 240 {
        out.truncate(240);
    }
    if out.trim().is_empty() {
        "failed".to_string()
    } else {
        out
    }
}

pub(crate) fn http_404_response(path: &str) -> Response {
    axum::http::Response::builder()
        .status(axum::http::StatusCode::NOT_FOUND)
        .header("content-type", "application/json")
        .body(Body::from(format!(
            "{{\"error\":\"no route matches\",\"path\":\"{path}\"}}"
        )))
        .expect("response builder")
}
