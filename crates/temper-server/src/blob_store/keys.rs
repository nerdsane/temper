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

/// The first key segment of every tenant's namespace but `default`'s: such a tenant's objects sit under
/// `tenants/<sha256 of its name>` in the store `default` keeps its own at the root of.
pub(crate) const TENANT_NAMESPACE_SEGMENT: &str = "tenants";

/// The key itself, if it addresses an object inside the namespace it is resolved in; every blob read and write
/// resolves its key through this, on a local disk and in an object store alike.
///
/// Tenant namespaces are disjoint: a key never starts with [`TENANT_NAMESPACE_SEGMENT`] (in any case, since a local
/// disk may fold case), so `default`'s keys never reach another tenant's, and no segment climbs out of the
/// namespace. An object store's address is a URL, which folds `.`, `..` and their percent-encoded forms and reads a
/// backslash as `/`, `?` as the query and `#` as the fragment, so none of those may appear either.
pub(crate) fn checked_object_key(key: &str) -> Result<&str, String> {
    let invalid = || format!("invalid blob key '{key}'");
    let mut segments = key.split('/');
    let first = segments.next().unwrap_or_default();
    if first.eq_ignore_ascii_case(TENANT_NAMESPACE_SEGMENT) {
        return Err(format!(
            "blob key '{key}' starts with '{TENANT_NAMESPACE_SEGMENT}', where tenants' own namespaces are; nothing was read or written"
        ));
    }
    for segment in std::iter::once(first).chain(segments) {
        if segment.is_empty()
            || segment == "."
            || segment == ".."
            || segment.contains(['\\', '%', '?', '#'])
            || segment.chars().any(char::is_control)
        {
            return Err(invalid());
        }
    }
    Ok(key)
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

#[cfg(test)]
mod tests {
    use super::checked_object_key;

    #[test]
    fn a_key_stays_inside_the_namespace_it_is_resolved_in() {
        for key in [
            "tenants/abc/x",
            "Tenants/abc/x",
            "TENANTS",
            "",
            "/x",
            "a//b",
            "./x",
            "a/../../x",
            "..",
            "a/%2e%2e/x",
            "a/.%2E/x",
            "a\\..\\x",
            "a/x?versionId=1",
            "a/x#y",
            "a/\nx",
        ] {
            assert!(checked_object_key(key).is_err(), "{key:?} is refused");
        }
        for key in [
            "x",
            "temper-fs/sha256:ab",
            "field-overflow/sha256/ab.json",
            "a/tenants/x",
            "tenant/x",
            "published-artifacts/v1.2/x",
        ] {
            assert_eq!(checked_object_key(key), Ok(key), "{key:?} is kept as it is");
        }
    }
}
