//! Exact classification for the server-owned loopback blob endpoint.

const INTERNAL_BLOB_PATH: &str = "/_internal/blobs";

/// Parsed loopback blob endpoint used to bind a WASM host capability.
///
/// Keeping the parsed origin and path together prevents a loose localhost or
/// substring check from granting HTTP authority to a different local service.
#[derive(Clone, Debug)]
pub(crate) struct LocalInternalBlobEndpoint {
    base: reqwest::Url,
}

impl LocalInternalBlobEndpoint {
    /// Parse only the exact server-owned endpoint base.
    pub(crate) fn parse(endpoint: &str) -> Option<Self> {
        let mut base = reqwest::Url::parse(endpoint).ok()?;
        if base.scheme() != "http"
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
            || base.port().is_none()
            || !is_loopback_host(base.host_str()?)
        {
            return None;
        }
        let normalized_path = base.path().trim_end_matches('/');
        if normalized_path != INTERNAL_BLOB_PATH {
            return None;
        }
        base.set_path(INTERNAL_BLOB_PATH);
        Some(Self { base })
    }

    /// Return the canonical object key when `url` belongs to this exact bound
    /// endpoint. Near-match paths, other ports, credentials, queries, and
    /// fragments are rejected.
    pub(crate) fn object_key(&self, url: &str) -> Option<String> {
        let request = reqwest::Url::parse(url).ok()?;
        if request.scheme() != self.base.scheme()
            || request.host_str() != self.base.host_str()
            || request.port_or_known_default() != self.base.port_or_known_default()
            || !request.username().is_empty()
            || request.password().is_some()
            || request.query().is_some()
            || request.fragment().is_some()
        {
            return None;
        }
        let key = request
            .path()
            .strip_prefix(&format!("{INTERNAL_BLOB_PATH}/"))?;
        (!key.is_empty()).then(|| key.to_string())
    }
}

pub(crate) fn is_local_internal_blob_endpoint(endpoint: &str) -> bool {
    LocalInternalBlobEndpoint::parse(endpoint).is_some()
}

fn is_loopback_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "::1" | "[::1]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_base_requires_exact_loopback_origin_and_path() {
        for endpoint in [
            "http://127.0.0.1:3000/_internal/blobs",
            "http://localhost:3000/_internal/blobs/",
            "http://[::1]:3000/_internal/blobs",
        ] {
            assert!(
                LocalInternalBlobEndpoint::parse(endpoint).is_some(),
                "{endpoint}"
            );
        }
        for endpoint in [
            "https://127.0.0.1:3000/_internal/blobs",
            "http://127.0.0.1/_internal/blobs",
            "http://127.0.0.1:3000/proxy/_internal/blobs",
            "http://127.0.0.1:3000/_internal/blobs.evil",
            "http://user@127.0.0.1:3000/_internal/blobs",
            "http://127.0.0.1:3000/_internal/blobs?mode=write",
        ] {
            assert!(
                LocalInternalBlobEndpoint::parse(endpoint).is_none(),
                "{endpoint}"
            );
        }
    }

    #[test]
    fn object_url_must_match_the_bound_origin_and_path() {
        let endpoint = LocalInternalBlobEndpoint::parse("http://127.0.0.1:3000/_internal/blobs")
            .expect("valid endpoint");
        assert_eq!(
            endpoint
                .object_key("http://127.0.0.1:3000/_internal/blobs/field-overflow/sha256/a.json",),
            Some("field-overflow/sha256/a.json".to_string())
        );
        for url in [
            "http://127.0.0.1:3001/_internal/blobs/key",
            "http://localhost:3000/_internal/blobs/key",
            "http://127.0.0.1:3000/proxy/_internal/blobs/key",
            "http://127.0.0.1:3000/_internal/blobs.evil/key",
            "http://127.0.0.1:3000/_internal/blobs/key?redirect=1",
            "http://user@127.0.0.1:3000/_internal/blobs/key",
        ] {
            assert!(endpoint.object_key(url).is_none(), "{url}");
        }
    }
}
