//! Genesis install security boundary (ADR-0157 / ARN-210).
//!
//! Hardens the registry install path against SSRF, path escape, unbounded
//! downloads, and unauthenticated management calls.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Component, Path, PathBuf};

/// Max compressed/raw bundle JSON bytes accepted from a registry.
pub const MAX_BUNDLE_RESPONSE_BYTES: u64 = 32 * 1024 * 1024;

/// Max apps in a single bundle closure.
pub const MAX_BUNDLE_APPS: usize = 64;

/// Max files across a single app package in a bundle.
pub const MAX_FILES_PER_APP: usize = 4_096;

/// Max decoded file payload size for one package file.
pub const MAX_FILE_BYTES: usize = 8 * 1024 * 1024;

/// Connect + total request timeout for registry fetches.
pub const REGISTRY_HTTP_TIMEOUT_SECS: u64 = 30;

/// Require verified platform-admin credentials for install mutations.
///
/// Client-supplied `X-Temper-Principal-Kind: admin` is **not** trusted by
/// itself (ARN-210). Admin is accepted only via:
/// 1. `Authorization: Bearer <TEMPER_PLATFORM_ADMIN_BEARER>` (preferred), or
/// 2. `Authorization: Bearer <TEMPER_API_KEY>` when that key is configured, or
/// 3. Explicit dev opt-in `TEMPER_GENESIS_INSTALL_DEV_ADMIN=1` **and**
///    `X-Temper-Principal-Kind: admin` (local tests only).
pub fn require_platform_admin(
    headers: &axum::http::HeaderMap,
) -> Result<(), axum::http::StatusCode> {
    let auth = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if let Ok(expected) = std::env::var("TEMPER_PLATFORM_ADMIN_BEARER") {
        let expected = expected.trim();
        if !expected.is_empty() {
            let want = format!("Bearer {expected}");
            return if constant_time_eq(auth.as_bytes(), want.as_bytes()) {
                Ok(())
            } else {
                Err(axum::http::StatusCode::UNAUTHORIZED)
            };
        }
    }

    if let Ok(api_key) = std::env::var("TEMPER_API_KEY") {
        let api_key = api_key.trim();
        if !api_key.is_empty() {
            let want = format!("Bearer {api_key}");
            return if constant_time_eq(auth.as_bytes(), want.as_bytes()) {
                Ok(())
            } else {
                Err(axum::http::StatusCode::UNAUTHORIZED)
            };
        }
    }

    // Dev/test only — never the production path.
    if matches!(
        std::env::var("TEMPER_GENESIS_INSTALL_DEV_ADMIN")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes"
    ) {
        let kind = headers
            .get("x-temper-principal-kind")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if matches!(kind.as_str(), "admin" | "system" | "platform-admin") {
            return Ok(());
        }
    }

    Err(axum::http::StatusCode::UNAUTHORIZED)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    // Always walk the expected secret length so timing does not leak it.
    let mut diff = if a.len() == b.len() { 0u8 } else { 1u8 };
    for (i, y) in b.iter().enumerate() {
        let x = a.get(i).copied().unwrap_or(0);
        diff |= x ^ *y;
    }
    diff == 0
}

/// Normalize and validate a registry base URL against the SSRF policy.
pub fn normalize_and_validate_registry_url(raw: &str) -> Result<String, String> {
    let fallback = std::env::var("TEMPER_GENESIS_REGISTRY_URL").unwrap_or_default();
    let raw = if raw.trim().is_empty() {
        fallback.as_str()
    } else {
        raw
    };
    let value = raw.trim().trim_end_matches('/');
    if value.is_empty() {
        return Err("registry_url is required for Genesis app install".to_string());
    }

    let parsed =
        reqwest::Url::parse(value).map_err(|e| format!("registry_url is not a valid URL: {e}"))?;
    let scheme = parsed.scheme();
    if scheme != "https" && scheme != "http" {
        return Err("registry_url must use http:// or https://".to_string());
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| "registry_url must include a host".to_string())?;

    let allow_loopback = matches!(
        std::env::var("TEMPER_GENESIS_ALLOW_HTTP_LOOPBACK")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes"
    );

    // Loopback (localhost / 127.0.0.1 / ::1) is opt-in only — including https.
    if is_loopback_host(host) {
        if !allow_loopback {
            return Err(
                "registry_url loopback host is blocked (set TEMPER_GENESIS_ALLOW_HTTP_LOOPBACK for local dev)"
                    .to_string(),
            );
        }
    } else {
        // Non-loopback: https only + block private/metadata literals + bad hostnames.
        if scheme != "https" {
            return Err(
                "registry_url must use https:// (http loopback only with TEMPER_GENESIS_ALLOW_HTTP_LOOPBACK)"
                    .to_string(),
            );
        }
        if let Ok(ip) = host.parse::<IpAddr>() {
            if is_blocked_ip(ip) {
                return Err(format!(
                    "registry_url host resolves to a blocked address class: {ip}"
                ));
            }
        } else if is_blocked_hostname(host) {
            return Err(format!("registry_url host '{host}' is not allowed"));
        }
    }

    if let Some(allowlist) = registry_allowlist() {
        let ok = allowlist
            .iter()
            .any(|entry| host.eq_ignore_ascii_case(entry));
        if !ok {
            return Err(format!(
                "registry host '{host}' is not in TEMPER_GENESIS_REGISTRY_ALLOWLIST"
            ));
        }
    }

    // Reject userinfo (credential stuffing / SSRF obfuscation).
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("registry_url must not include userinfo".to_string());
    }

    Ok(value.to_string())
}

fn registry_allowlist() -> Option<Vec<String>> {
    let raw = std::env::var("TEMPER_GENESIS_REGISTRY_ALLOWLIST").ok()?;
    let list: Vec<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect();
    if list.is_empty() { None } else { Some(list) }
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
        || (ip.octets()[0] == 100 && (ip.octets()[1] & 0xc0) == 64)
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

/// Validate a remote app package name before joining under the cache root.
pub fn safe_app_package_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("bundle app name must not be empty".to_string());
    }
    if name.len() > 128 {
        return Err("bundle app name exceeds 128 characters".to_string());
    }
    if name == "." || name == ".." {
        return Err("bundle app name must not be '.' or '..'".to_string());
    }
    if name.contains('/') || name.contains('\\') || name.contains('\0') {
        return Err("bundle app name must not contain path separators".to_string());
    }
    if Path::new(name).is_absolute() {
        return Err("bundle app name must be a relative package id".to_string());
    }
    for component in Path::new(name).components() {
        match component {
            Component::Normal(part) => {
                let s = part.to_string_lossy();
                if !s
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
                {
                    return Err(format!(
                        "bundle app name '{name}' contains forbidden characters"
                    ));
                }
            }
            _ => {
                return Err(format!(
                    "bundle app name '{name}' must not contain path components like '..'"
                ));
            }
        }
    }
    Ok(name.to_string())
}

/// Join `name` under `cache_root` only if the result stays inside the root.
pub fn join_under_cache(cache_root: &Path, name: &str) -> Result<PathBuf, String> {
    let safe = safe_app_package_name(name)?;
    let joined = cache_root.join(&safe);
    let root_canon = cache_root
        .canonicalize()
        .unwrap_or_else(|_| cache_root.to_path_buf());
    if !joined.starts_with(cache_root) {
        return Err(format!(
            "bundle app path escapes cache root: {}",
            joined.display()
        ));
    }
    if let Ok(parent) = joined.parent().unwrap_or(cache_root).canonicalize()
        && !parent.starts_with(&root_canon)
        && parent != root_canon
    {
        return Err(format!(
            "bundle app path escapes cache root after resolution: {}",
            joined.display()
        ));
    }
    Ok(joined)
}

/// Collision-resistant cache key fragment (readable prefix + hash suffix).
pub fn collision_resistant_cache_key(app_ref: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(app_ref.as_bytes());
    let digest = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let mut readable = String::new();
    let mut last_dash = false;
    for ch in app_ref.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            readable.push(ch);
            last_dash = false;
        } else if !last_dash {
            readable.push('-');
            last_dash = true;
        }
    }
    let readable = readable.trim_matches('-');
    let readable = if readable.is_empty() {
        "app"
    } else if readable.len() > 48 {
        &readable[..48]
    } else {
        readable
    };
    format!("{readable}-{}", &digest[..16])
}

/// Read a response body with a hard byte cap (abort before unbounded alloc).
pub async fn read_body_capped(
    response: reqwest::Response,
    max_bytes: u64,
) -> Result<Vec<u8>, String> {
    if let Some(len) = response.content_length()
        && len > max_bytes
    {
        return Err(format!(
            "response Content-Length {len} exceeds max {max_bytes} bytes"
        ));
    }
    use futures_util::StreamExt;
    let mut stream = response.bytes_stream();
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("read response body: {e}"))?;
        let next = buf.len() as u64 + chunk.len() as u64;
        if next > max_bytes {
            return Err(format!(
                "response body exceeds max {max_bytes} bytes (aborting stream)"
            ));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue, StatusCode};

    #[test]
    fn admin_requires_verified_bearer_not_spoofed_header() {
        let mut headers = HeaderMap::new();
        // No secrets configured → fail closed.
        // SAFETY: single-threaded unit test env mutation.
        unsafe {
            std::env::remove_var("TEMPER_PLATFORM_ADMIN_BEARER");
            std::env::remove_var("TEMPER_API_KEY");
            std::env::remove_var("TEMPER_GENESIS_INSTALL_DEV_ADMIN");
        }
        assert_eq!(
            require_platform_admin(&headers),
            Err(StatusCode::UNAUTHORIZED)
        );

        // Spoofed admin header alone is denied.
        headers.insert("x-temper-principal-kind", HeaderValue::from_static("admin"));
        assert_eq!(
            require_platform_admin(&headers),
            Err(StatusCode::UNAUTHORIZED)
        );

        // Correct bearer accepted.
        // SAFETY: test-only env mutation in single-threaded unit test.
        unsafe {
            std::env::set_var("TEMPER_PLATFORM_ADMIN_BEARER", "super-secret");
        }
        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer super-secret"),
        );
        assert_eq!(require_platform_admin(&headers), Ok(()));

        // Wrong bearer denied even with admin header.
        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer wrong"),
        );
        assert_eq!(
            require_platform_admin(&headers),
            Err(StatusCode::UNAUTHORIZED)
        );
        unsafe {
            std::env::remove_var("TEMPER_PLATFORM_ADMIN_BEARER");
        }
    }

    #[test]
    fn rejects_private_metadata_and_localhost_registry_urls() {
        // SAFETY: single-threaded unit test env mutation.
        unsafe {
            std::env::remove_var("TEMPER_GENESIS_ALLOW_HTTP_LOOPBACK");
        }
        assert!(normalize_and_validate_registry_url("https://127.0.0.1/genesis").is_err());
        assert!(normalize_and_validate_registry_url("https://localhost/genesis").is_err());
        assert!(normalize_and_validate_registry_url("https://10.0.0.5/g").is_err());
        assert!(normalize_and_validate_registry_url("https://192.168.1.1/g").is_err());
        assert!(normalize_and_validate_registry_url("https://169.254.169.254/latest").is_err());
        assert!(normalize_and_validate_registry_url("http://evil.example/g").is_err());
        assert!(normalize_and_validate_registry_url("https://user:pass@evil.example/g").is_err());
    }

    #[test]
    fn accepts_public_https_registry() {
        let url = normalize_and_validate_registry_url("https://genesis.example.com/registry/")
            .expect("public https should be accepted");
        assert_eq!(url, "https://genesis.example.com/registry");
    }

    #[test]
    fn app_name_traversal_rejected() {
        assert!(safe_app_package_name("../escape").is_err());
        assert!(safe_app_package_name("/tmp/x").is_err());
        assert!(safe_app_package_name("a/b").is_err());
        assert!(safe_app_package_name("..").is_err());
        assert_eq!(safe_app_package_name("notes-app").unwrap(), "notes-app");
    }

    #[test]
    fn join_under_cache_blocks_escape() {
        let root = std::env::temp_dir().join("temper-genesis-cache-test");
        let _ = std::fs::create_dir_all(&root);
        assert!(join_under_cache(&root, "../escape").is_err());
        let ok = join_under_cache(&root, "notes").unwrap();
        assert!(ok.starts_with(&root));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cache_keys_do_not_collide_across_distinct_refs() {
        let a = collision_resistant_cache_key("owner/app@aaaa");
        let b = collision_resistant_cache_key("owner/app@bbbb");
        let c = collision_resistant_cache_key("owner/app-aaaa");
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert!(a.contains('-'));
        assert!(a.len() > 16);
    }
}
