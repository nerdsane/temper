//! AWS Signature Version 4 request signing shared by blob transports.
//!
//! Owns the SigV4 primitives and the canonical-request/header recipe used by
//! both the tenant blob store (`blob_store`) and public artifact publishing
//! (`state::published_artifacts`). Per-site header-shape differences (payload
//! hashing policy, extra signed headers such as `content-type`) stay at the
//! call site via [`SignedHeaderRequest`].

use hmac::{Hmac, Mac};
use reqwest::header::{AUTHORIZATION, HOST, HeaderMap, HeaderValue};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// Inputs for [`build_signed_headers`].
pub(crate) struct SignedHeaderRequest<'a> {
    /// HTTP method of the request being signed (e.g. `PUT`).
    pub method: &'a str,
    /// Full request URL; the signed host and canonical URI derive from it.
    pub url: &'a str,
    /// Hex SHA-256 of the request body, or `UNSIGNED-PAYLOAD`.
    pub payload_hash: &'a str,
    /// Region component of the credential scope.
    pub region: &'a str,
    /// Service component of the credential scope.
    pub service: &'a str,
    /// Access key id for the `Credential` clause.
    pub access_key: &'a str,
    /// Secret key the signing key derives from.
    pub secret_key: &'a str,
    /// Request timestamp in `%Y%m%dT%H%M%SZ` form (see [`amz_date_now`]).
    pub amz_date: &'a str,
    /// Signed headers beyond the standard `host` / `x-amz-content-sha256` /
    /// `x-amz-date` trio. Names must be lowercase; the caller remains
    /// responsible for sending these headers on the actual request.
    pub extra_signed_headers: &'a [(&'a str, &'a str)],
    /// Label for header-construction error messages (e.g. `blob`).
    pub error_context: &'a str,
}

/// Current UTC timestamp in the `x-amz-date` format SigV4 requires.
pub(crate) fn amz_date_now() -> String {
    chrono::Utc::now() // determinism-ok: SigV4 signing requires real wall-clock time
        .format("%Y%m%dT%H%M%SZ")
        .to_string()
}

/// Build the SigV4-signed header set for a request.
///
/// Returns `authorization`, `x-amz-date`, `x-amz-content-sha256`, and `host`.
/// Extra signed headers participate in the signature but are not inserted
/// into the returned map.
pub(crate) fn build_signed_headers(request: &SignedHeaderRequest<'_>) -> Result<HeaderMap, String> {
    let date = &request.amz_date[..8];
    let (host, path) = parse_url_host_path(request.url);
    let canonical_uri = uri_encode_path(path);
    let scope = format!("{date}/{}/{}/aws4_request", request.region, request.service);

    let mut header_pairs: Vec<(&str, &str)> = vec![
        ("host", host),
        ("x-amz-content-sha256", request.payload_hash),
        ("x-amz-date", request.amz_date),
    ];
    header_pairs.extend_from_slice(request.extra_signed_headers);
    header_pairs.sort_by(|a, b| a.0.cmp(b.0));

    let canonical_headers: String = header_pairs
        .iter()
        .map(|(name, value)| format!("{name}:{value}\n"))
        .collect();
    let signed_headers = header_pairs
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(";");
    let canonical_request = format!(
        "{}\n{canonical_uri}\n\n{canonical_headers}\n{signed_headers}\n{}",
        request.method, request.payload_hash
    );
    let canonical_request_hash = sha256_hex(canonical_request.as_bytes());
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{scope}\n{canonical_request_hash}",
        request.amz_date
    );
    let signing_key = derive_signing_key(request.secret_key, date, request.region, request.service);
    let signature = hex_encode(&hmac_sha256(&signing_key, string_to_sign.as_bytes()));
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope},SignedHeaders={signed_headers},Signature={signature}",
        request.access_key
    );

    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&authorization).map_err(|e| {
            format!(
                "invalid {} authorization header: {e}",
                request.error_context
            )
        })?,
    );
    headers.insert(
        "x-amz-date",
        HeaderValue::from_str(request.amz_date)
            .map_err(|e| format!("invalid x-amz-date header: {e}"))?,
    );
    headers.insert(
        "x-amz-content-sha256",
        HeaderValue::from_str(request.payload_hash)
            .map_err(|e| format!("invalid x-amz-content-sha256 header: {e}"))?,
    );
    headers.insert(
        HOST,
        HeaderValue::from_str(host)
            .map_err(|e| format!("invalid {} host header: {e}", request.error_context))?,
    );
    Ok(headers)
}

/// Hex-encoded SHA-256 digest of `data`.
pub(crate) fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex_encode(&hasher.finalize())
}

/// Derive the SigV4 signing key from the secret key and credential scope.
pub(crate) fn derive_signing_key(
    secret_key: &str,
    date: &str,
    region: &str,
    service: &str,
) -> Vec<u8> {
    let k_secret = format!("AWS4{secret_key}");
    let k_date = hmac_sha256(k_secret.as_bytes(), date.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    hmac_sha256(&k_service, b"aws4_request")
}

/// HMAC-SHA256 of `data` under `key`.
pub(crate) fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// Lowercase hex encoding of `bytes`.
pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Split a URL into `(host, path)`, defaulting the path to `/`.
pub(crate) fn parse_url_host_path(url: &str) -> (&str, &str) {
    let after_scheme = if let Some(pos) = url.find("://") {
        &url[pos + 3..]
    } else {
        url
    };
    if let Some(slash) = after_scheme.find('/') {
        (&after_scheme[..slash], &after_scheme[slash..])
    } else {
        (after_scheme, "/")
    }
}

/// Percent-encode a URL path per the SigV4 canonical URI rules
/// (`/` is preserved; unreserved characters pass through).
pub(crate) fn uri_encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len() + 16);
    for byte in path.bytes() {
        match byte {
            b'/' | b'-' | b'_' | b'.' | b'~' => out.push(byte as char),
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' => out.push(byte as char),
            _ => {
                out.push('%');
                out.push(b"0123456789ABCDEF"[(byte >> 4) as usize] as char);
                out.push(b"0123456789ABCDEF"[(byte & 0x0f) as usize] as char);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The signing-key derivation vector published in the AWS SigV4
    /// documentation ("Examples of how to derive a signing key").
    #[test]
    fn derive_signing_key_matches_aws_documented_vector() {
        let key = derive_signing_key(
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "20150830",
            "us-east-1",
            "iam",
        );
        assert_eq!(
            hex_encode(&key),
            "c4afb1cc5771d871763a393e44b703571b55cc28424d1a5e86da6ed3c154a4b9"
        );
    }

    #[test]
    fn sha256_hex_empty_input_is_the_well_known_digest() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn uri_encode_path_preserves_slashes_and_escapes_the_rest() {
        assert_eq!(uri_encode_path("/a b/c~d.txt"), "/a%20b/c~d.txt");
        assert_eq!(uri_encode_path("/plain/path"), "/plain/path");
    }

    /// Golden pin for the full header recipe with fixed inputs. The
    /// signature value was captured from this implementation after an
    /// external byte-equivalence proof against the two pre-extraction
    /// copies; any change to the canonical-request construction breaks it.
    #[test]
    fn build_signed_headers_golden_vector() {
        let headers = build_signed_headers(&SignedHeaderRequest {
            method: "PUT",
            url: "https://blobs.example.com/tenant/a b/object.bin",
            payload_hash: "UNSIGNED-PAYLOAD",
            region: "auto",
            service: "s3",
            access_key: "AKIDEXAMPLE",
            secret_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            amz_date: "20150830T123600Z",
            extra_signed_headers: &[("content-type", "application/octet-stream")],
            error_context: "test",
        })
        .expect("signing succeeds");

        assert_eq!(headers["host"], "blobs.example.com");
        assert_eq!(headers["x-amz-date"], "20150830T123600Z");
        assert_eq!(headers["x-amz-content-sha256"], "UNSIGNED-PAYLOAD");
        let authorization = headers["authorization"].to_str().unwrap();
        assert!(authorization.starts_with(
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/auto/s3/aws4_request,\
             SignedHeaders=content-type;host;x-amz-content-sha256;x-amz-date,Signature="
        ));
        let signature = authorization.rsplit("Signature=").next().unwrap();
        assert_eq!(signature.len(), 64, "signature is a 32-byte hex digest");
        assert_eq!(
            signature,
            "c0fcfd599eca87021d40732416afb30d604bfac09c6698dcbfb7629655bff2f5"
        );
    }
}
