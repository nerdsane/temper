//! Canonical object-store key construction.

const WASM_ARTIFACT_PREFIX: &str = "wasm-modules/";

/// Where the kernel keeps its own records in the blob table, such as a
/// check's kept verdict. No caller's blob is written under it: a caller who
/// could write `kernel/check-verdict/<digest>` would pass a spec no check
/// passed.
pub const KERNEL_BLOB_PREFIX: &str = "kernel/";

/// Whether the kernel owns `key`: its own records, and modules' bytes, which
/// it kept under wasm-modules/ before kernel/ existed. Only the kernel writes
/// a key it owns, so what it reads there is what it wrote.
pub(crate) fn kernel_owns(key: &str) -> bool {
    key.starts_with(KERNEL_BLOB_PREFIX) || key.starts_with(WASM_ARTIFACT_PREFIX)
}
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
