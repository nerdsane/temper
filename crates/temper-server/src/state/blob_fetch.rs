use std::sync::OnceLock;

use hmac::{Hmac, Mac};
use reqwest::header::{AUTHORIZATION, HOST, HeaderMap, HeaderValue};
use sha2::{Digest, Sha256};
use temper_runtime::tenant::TenantId;

use super::ServerState;

type HmacSha256 = Hmac<Sha256>;

pub(super) async fn fetch_external_blob_bytes(
    state: &ServerState,
    tenant: &TenantId,
    endpoint: &str,
    content_hash: &str,
    bucket: &str,
) -> Result<Option<Vec<u8>>, String> {
    let url = format!(
        "{}/{}/{}",
        endpoint.trim_end_matches('/'),
        bucket.trim_matches('/'),
        content_hash
    );
    let mut request = blob_http_client().get(&url);
    let headers = build_blob_get_headers(state, tenant, &url)?;
    for (header_name, header_value) in &headers {
        request = request.header(header_name, header_value);
    }

    let response = request
        .send()
        .await
        .map_err(|e| format!("blob GET request failed for '{content_hash}': {e}"))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !response.status().is_success() {
        return Err(format!(
            "blob GET failed for '{content_hash}' with HTTP {}",
            response.status()
        ));
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|e| format!("blob GET body read failed for '{content_hash}': {e}"))?;
    Ok(Some(bytes.to_vec()))
}

fn blob_http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(reqwest::Client::new)
}

fn build_blob_get_headers(
    state: &ServerState,
    tenant: &TenantId,
    url: &str,
) -> Result<HeaderMap, String> {
    let mut headers = HeaderMap::new();
    let Some(vault) = state.secrets_vault.as_ref() else {
        return Ok(headers);
    };

    let Some(access_key) = vault.get_secret(tenant.as_str(), "blob_access_key") else {
        return Ok(headers);
    };
    let Some(secret_key) = vault.get_secret(tenant.as_str(), "blob_secret_key") else {
        return Ok(headers);
    };

    let datetime = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let date = &datetime[..8];
    let (host, path) = parse_url_host_path(url);
    let canonical_uri = uri_encode_path(path);
    let payload_hash = "UNSIGNED-PAYLOAD";
    let region = "auto";
    let service = "s3";
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let canonical_headers =
        format!("host:{host}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{datetime}\n");
    let signed_headers = "host;x-amz-content-sha256;x-amz-date";
    let canonical_request =
        format!("GET\n{canonical_uri}\n\n{canonical_headers}\n{signed_headers}\n{payload_hash}");
    let canonical_request_hash = sha256_hex(canonical_request.as_bytes());
    let string_to_sign = format!("AWS4-HMAC-SHA256\n{datetime}\n{scope}\n{canonical_request_hash}");
    let signing_key = derive_signing_key(&secret_key, date, region, service);
    let signature = hex_encode(&hmac_sha256(&signing_key, string_to_sign.as_bytes()));
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={access_key}/{scope},SignedHeaders={signed_headers},Signature={signature}"
    );

    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&authorization)
            .map_err(|e| format!("invalid blob authorization header: {e}"))?,
    );
    headers.insert(
        "x-amz-date",
        HeaderValue::from_str(&datetime).map_err(|e| format!("invalid x-amz-date header: {e}"))?,
    );
    headers.insert(
        "x-amz-content-sha256",
        HeaderValue::from_static(payload_hash),
    );
    headers.insert(
        HOST,
        HeaderValue::from_str(host).map_err(|e| format!("invalid blob host header: {e}"))?,
    );
    Ok(headers)
}

fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex_encode(&hasher.finalize())
}

fn derive_signing_key(secret_key: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    let k_secret = format!("AWS4{secret_key}");
    let k_date = hmac_sha256(k_secret.as_bytes(), date.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    hmac_sha256(&k_service, b"aws4_request")
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn parse_url_host_path(url: &str) -> (&str, &str) {
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

fn uri_encode_path(path: &str) -> String {
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
