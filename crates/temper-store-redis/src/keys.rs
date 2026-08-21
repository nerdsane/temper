//! Redis key naming conventions for Temper.
//!
//! All keys are prefixed with `temper:` to namespace within shared Redis
//! instances. Subsystem-specific key builders live next to their subsystem;
//! this module holds only the shared prefix.

use temper_runtime::persistence::PersistenceError;

/// Key prefix for all Temper Redis keys.
pub const PREFIX: &str = "temper";

pub(crate) fn encode_lex_component(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(value.len().saturating_mul(2));
    for byte in value.as_bytes() {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

pub(crate) fn decode_lex_component(value: &str) -> Result<String, PersistenceError> {
    if !value.len().is_multiple_of(2) {
        return Err(PersistenceError::Serialization(
            "invalid Redis journal index component".to_string(),
        ));
    }
    let mut decoded = Vec::with_capacity(value.len() / 2);
    for pair in value.as_bytes().chunks_exact(2) {
        let high = decode_hex_digit(pair[0])?;
        let low = decode_hex_digit(pair[1])?;
        decoded.push((high << 4) | low);
    }
    String::from_utf8(decoded).map_err(|error| PersistenceError::Serialization(error.to_string()))
}

fn decode_hex_digit(digit: u8) -> Result<u8, PersistenceError> {
    match digit {
        b'0'..=b'9' => Ok(digit - b'0'),
        b'a'..=b'f' => Ok(digit - b'a' + 10),
        _ => Err(PersistenceError::Serialization(
            "invalid Redis journal index component".to_string(),
        )),
    }
}
