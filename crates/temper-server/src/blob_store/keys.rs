//! Canonical object-store key construction.

const WASM_ARTIFACT_PREFIX: &str = "wasm-modules/";
pub(crate) const DEFAULT_BLOB_BUCKET: &str = "temper-fs";

pub(crate) fn wasm_artifact_key(sha256_hash: &str) -> String {
    format!("{WASM_ARTIFACT_PREFIX}{sha256_hash}")
}

pub(crate) fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}
