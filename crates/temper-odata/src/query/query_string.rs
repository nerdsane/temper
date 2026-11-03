//! Split a raw URL query string into percent-decoded key/value pairs.
//!
//! Splitting happens before decoding, so an encoded `&`, `=` or parenthesis
//! inside a value stays part of that value.

use crate::error::ODataError;

/// Split a raw query string (without leading `?`) into decoded pairs.
///
/// Parameters are separated by `&` outside parentheses, and each key is
/// separated from its value by the first `=`. Both are then decoded the way
/// an HTTP form decoder reads a query string: `+` is a space and `%XX` is a
/// byte, with invalid UTF-8 replaced.
pub(super) fn split_query_string(qs: &str) -> Result<Vec<(String, String)>, ODataError> {
    split_query_params(qs)
        .iter()
        .map(|pair| {
            let (key, value) = split_key_value(pair)?;
            Ok((decode_component(key), decode_component(value)))
        })
        .collect()
}

/// Split query string parameters, respecting parentheses (for nested $expand).
fn split_query_params(qs: &str) -> Vec<String> {
    let mut params = Vec::new();
    let mut current = String::new();
    let mut paren_depth: u32 = 0;

    for ch in qs.chars() {
        match ch {
            '(' => {
                paren_depth += 1;
                current.push(ch);
            }
            ')' => {
                paren_depth = paren_depth.saturating_sub(1);
                current.push(ch);
            }
            '&' if paren_depth == 0 => {
                if !current.is_empty() {
                    params.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        params.push(current);
    }
    params
}

/// Split one `key=value` option at its first `=`, trimming the key.
pub(super) fn split_key_value(pair: &str) -> Result<(&str, &str), ODataError> {
    match pair.split_once('=') {
        Some((key, value)) => Ok((key.trim(), value)),
        None => Err(ODataError::InvalidQueryOption {
            option: pair.to_string(),
            message: "missing '=' in query parameter".into(),
        }),
    }
}

/// Decode one query-string component: `+` becomes a space, then `%XX` is
/// percent-decoded.
fn decode_component(component: &str) -> String {
    let spaced = component.replace('+', " ");
    percent_encoding::percent_decode_str(&spaced)
        .decode_utf8_lossy()
        .into_owned()
}
