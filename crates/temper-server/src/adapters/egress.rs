//! HTTP adapter egress policy (ADR-0156 / ARN-228).
//!
//! Fail-closed URL validation for native HTTP integrations: block private /
//! metadata / link-local destinations, require https unless loopback is
//! explicitly opted in, and reject userinfo.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Default total request timeout for adapter HTTP calls.
pub const ADAPTER_HTTP_TIMEOUT_SECS: u64 = 30;

/// Max response body bytes accepted from an adapter HTTP call.
pub const ADAPTER_MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// Validate an adapter HTTP URL against the SSRF / private-address policy.
///
/// On success returns the trimmed URL string suitable for `reqwest`.
pub fn validate_adapter_http_url(raw: &str) -> Result<String, String> {
    let value = raw.trim();
    if value.is_empty() {
        return Err("adapter url must not be empty".to_string());
    }

    let parsed =
        reqwest::Url::parse(value).map_err(|e| format!("adapter url is not a valid URL: {e}"))?;
    let scheme = parsed.scheme();
    if scheme != "https" && scheme != "http" {
        return Err("adapter url must use http:// or https://".to_string());
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| "adapter url must include a host".to_string())?;

    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("adapter url must not include userinfo".to_string());
    }

    let allow_loopback = adapter_allow_http_loopback();

    if is_loopback_host(host) {
        if !allow_loopback {
            return Err(
                "adapter url loopback host is blocked (set TEMPER_ADAPTER_ALLOW_HTTP_LOOPBACK for local dev/tests)"
                    .to_string(),
            );
        }
        // Loopback opt-in still rejects non-http(s) (already checked) and allows
        // either scheme for local mock servers.
        return Ok(value.to_string());
    }

    if scheme != "https" {
        return Err(
            "adapter url must use https:// (http loopback only with TEMPER_ADAPTER_ALLOW_HTTP_LOOPBACK)"
                .to_string(),
        );
    }

    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_blocked_ip(ip) {
            return Err(format!(
                "adapter url host resolves to a blocked address class: {ip}"
            ));
        }
    } else if is_blocked_hostname(host) {
        return Err(format!("adapter url host '{host}' is not allowed"));
    }

    Ok(value.to_string())
}

fn adapter_allow_http_loopback() -> bool {
    // determinism-ok: process-level ops flag read at request time; not a sim clock source.
    matches!(
        std::env::var("TEMPER_ADAPTER_ALLOW_HTTP_LOOPBACK")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes"
    )
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host == "127.0.0.1"
        || host == "::1"
        || host == "[::1]"
        || host.parse::<IpAddr>().is_ok_and(|ip| match ip {
            IpAddr::V4(v4) => v4.is_loopback(),
            IpAddr::V6(v6) => v6.is_loopback(),
        })
}

fn is_blocked_hostname(host: &str) -> bool {
    let h = host.to_ascii_lowercase();
    h == "localhost"
        || matches!(
            h.as_str(),
            "metadata.google.internal"
                | "metadata"
                | "kubernetes.default"
                | "kubernetes.default.svc"
                | "kubernetes.default.svc.cluster.local"
        )
        || h.ends_with(".internal")
        || h.ends_with(".local")
        || h.ends_with(".localhost")
}

/// Blocked destination classes for SSRF (literal IPs in the URL).
pub fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_v4(v4),
        IpAddr::V6(v6) => is_blocked_v6(v6),
    }
}

fn is_blocked_v4(ip: Ipv4Addr) -> bool {
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_unspecified()
        || ip.octets()[0] == 0
        // CGNAT 100.64/10
        || (ip.octets()[0] == 100 && (ip.octets()[1] & 0xc0) == 64)
        // benchmarking 198.18/15
        || (ip.octets()[0] == 198 && (ip.octets()[1] == 18 || ip.octets()[1] == 19))
        || ip.is_multicast()
}

fn is_blocked_v6(ip: Ipv6Addr) -> bool {
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || ip.is_unique_local()
        || ip.is_unicast_link_local()
        || ip.to_ipv4_mapped().is_some_and(is_blocked_v4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_private_literal_ip() {
        let err = validate_adapter_http_url("https://10.0.0.5/hook").unwrap_err();
        assert!(err.contains("blocked"), "{err}");
    }

    #[test]
    fn rejects_metadata_hostname() {
        let err = validate_adapter_http_url("https://metadata.google.internal/").unwrap_err();
        assert!(
            err.contains("not allowed") || err.contains("blocked"),
            "{err}"
        );
    }

    #[test]
    fn rejects_userinfo() {
        let err = validate_adapter_http_url("https://user:pass@example.com/x").unwrap_err();
        assert!(err.contains("userinfo"), "{err}");
    }

    #[test]
    fn rejects_http_non_loopback_without_opt_in() {
        let err = validate_adapter_http_url("http://example.com/hook").unwrap_err();
        assert!(err.contains("https"), "{err}");
    }

    #[test]
    fn accepts_https_public_host() {
        let url = validate_adapter_http_url("https://hooks.example.com/path")
            .expect("public https host should be allowed");
        assert_eq!(url, "https://hooks.example.com/path");
    }

    #[test]
    fn loopback_host_and_ip_classified_without_env_mutation() {
        // Do not mutate process env here — cargo test runs unit tests on a pool.
        // Policy for opt-in is exercised via is_loopback_host / is_blocked_ip only.
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("::1"));
        assert!(is_loopback_host("[::1]"));
        assert!(!is_loopback_host("example.com"));
        assert!(is_blocked_ip(
            "127.0.0.1".parse().expect("parse loopback v4")
        ));
        assert!(is_blocked_ip("::1".parse().expect("parse loopback v6")));
    }

    #[test]
    fn blocked_ip_classes() {
        assert!(is_blocked_ip(
            "169.254.169.254"
                .parse()
                .expect("parse metadata link-local")
        ));
        assert!(is_blocked_ip(
            "192.168.1.1".parse().expect("parse private v4")
        ));
        assert!(is_blocked_ip("::1".parse().expect("parse loopback v6")));
        assert!(!is_blocked_ip("8.8.8.8".parse().expect("parse public v4")));
    }
}
