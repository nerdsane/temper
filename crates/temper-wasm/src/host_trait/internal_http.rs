//! Internal Temper HTTP request authentication and header isolation.

use std::sync::Arc;

/// One server-issued bearer capability for an exact internal HTTP request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InternalHttpCapability {
    bearer_token: String,
    tenant: String,
}

impl InternalHttpCapability {
    /// Construct a capability returned by a trusted server-side issuer.
    pub fn new(bearer_token: String, tenant: String) -> Result<Self, String> {
        if bearer_token.is_empty() {
            return Err("internal HTTP capability token must not be empty".to_string());
        }
        if tenant.is_empty() {
            return Err("internal HTTP capability tenant must not be empty".to_string());
        }
        Ok(Self {
            bearer_token,
            tenant,
        })
    }

    /// Opaque bearer value to send to the internal authentication edge.
    pub fn bearer_token(&self) -> &str {
        &self.bearer_token
    }

    /// Tenant bound into the capability.
    pub fn tenant(&self) -> &str {
        &self.tenant
    }
}

/// Callback that issues a fresh capability for one HTTP method and URL.
pub type InternalHttpCapabilityIssuerFn =
    Arc<dyn Fn(&str, &str) -> Result<InternalHttpCapability, String> + Send + Sync>;

/// Return whether `url` is inside the exact configured internal API origin and
/// path boundary.
pub(super) fn is_internal_url<'a>(url: &str, mut base_urls: impl Iterator<Item = &'a str>) -> bool {
    let Ok(request) = reqwest::Url::parse(url) else {
        return false;
    };
    if !request.username().is_empty() || request.password().is_some() {
        return false;
    }

    base_urls.any(|base| {
        let Ok(base) = reqwest::Url::parse(base) else {
            return false;
        };
        if request.scheme() != base.scheme()
            || request.host_str() != base.host_str()
            || request.port_or_known_default() != base.port_or_known_default()
        {
            return false;
        }

        let base_path = base.path().trim_end_matches('/');
        base_path.is_empty()
            || request.path() == base_path
            || request
                .path()
                .strip_prefix(base_path)
                .is_some_and(|suffix| suffix.starts_with('/'))
    })
}

/// Strip every guest-controlled authority input before internal re-entry.
///
/// Only ordinary transport/application headers and the explicitly
/// correlation-only Temper namespaces survive. The server then installs a
/// fresh bearer capability and its bound tenant.
pub(super) fn sanitize_internal_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    headers
        .iter()
        .filter(|(name, _)| internal_header_allowed(name))
        .cloned()
        .collect()
}

fn internal_header_allowed(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    if name.starts_with("x-temper-") {
        return name.starts_with("x-temper-workflow-") || name.starts_with("x-temper-observe-");
    }
    !matches!(
        name.as_str(),
        "authorization"
            | "proxy-authorization"
            | "cookie"
            | "host"
            | "forwarded"
            | "x-forwarded-for"
            | "x-forwarded-host"
            | "x-forwarded-proto"
            | "x-api-key"
            | "x-tenant-id"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internal_url_matching_uses_origin_and_path_boundaries() {
        let bases = ["https://temper.example/api"];
        for url in [
            "https://temper.example/api",
            "https://temper.example/api/tdata?x=1",
        ] {
            assert!(is_internal_url(url, bases.iter().copied()), "{url}");
        }
        for url in [
            "http://temper.example/api",
            "https://temper.example.evil/api",
            "https://temper.example:444/api",
            "https://user@temper.example/api",
            "https://temper.example/apix",
        ] {
            assert!(!is_internal_url(url, bases.iter().copied()), "{url}");
        }
    }

    #[test]
    fn sanitization_preserves_only_non_authority_and_correlation_headers() {
        let headers = vec![
            ("Authorization".to_string(), "Bearer guest".to_string()),
            ("X-Tenant-Id".to_string(), "victim".to_string()),
            ("Host".to_string(), "attacker".to_string()),
            ("X-Forwarded-For".to_string(), "127.0.0.1".to_string()),
            ("X-Temper-Principal-Kind".to_string(), "admin".to_string()),
            ("X-Temper-Attr-Limit".to_string(), "999".to_string()),
            ("X-Temper-Workflow-Run-Id".to_string(), "run-1".to_string()),
            (
                "X-Temper-Observe-Session-Id".to_string(),
                "session-1".to_string(),
            ),
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Idempotency-Key".to_string(), "request-1".to_string()),
            ("traceparent".to_string(), "00-trace-span-01".to_string()),
        ];

        assert_eq!(
            sanitize_internal_headers(&headers),
            vec![
                ("X-Temper-Workflow-Run-Id".to_string(), "run-1".to_string()),
                (
                    "X-Temper-Observe-Session-Id".to_string(),
                    "session-1".to_string()
                ),
                ("Content-Type".to_string(), "application/json".to_string()),
                ("Idempotency-Key".to_string(), "request-1".to_string()),
                ("traceparent".to_string(), "00-trace-span-01".to_string()),
            ]
        );
    }
}
