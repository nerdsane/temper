use hmac::{Hmac, Mac};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HOST, HeaderMap, HeaderValue};
use sha2::{Digest, Sha256};
use std::sync::OnceLock;
use temper_runtime::tenant::TenantId;
use temper_store_turso::{PublishedAssetRow, PublishedAssetUpsert};
use tracing::instrument;

use super::{IndexedFileStreamRead, ServerState};

const DEFAULT_PUBLIC_ASSET_PREFIX: &str = "published-assets";

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone)]
pub struct PublishFileAssetRequest {
    pub file_id: String,
    pub kind: String,
    pub owner_entity_type: String,
    pub owner_entity_id: String,
    pub source_file_version_id: String,
    pub public_key_prefix: Option<String>,
}

impl ServerState {
    #[instrument(skip_all, fields(
        otel.name = "state.publish_file_asset",
        tenant = %tenant,
        file_id = %request.file_id,
        kind = %request.kind,
        owner_entity_type = %request.owner_entity_type,
        owner_entity_id = %request.owner_entity_id,
    ))]
    pub async fn publish_file_asset(
        &self,
        tenant: &TenantId,
        request: PublishFileAssetRequest,
    ) -> Result<PublishedAssetRow, String> {
        let stream = self
            .read_file_stream_indexed(tenant, &request.file_id)
            .await?;
        let (content_hash, mime_type, bytes) = match stream {
            IndexedFileStreamRead::Content {
                content_hash,
                mime_type,
                bytes,
            } => (content_hash, mime_type, bytes),
            IndexedFileStreamRead::NoContent { .. } => {
                return Err(format!(
                    "File('{}') has no content to publish",
                    request.file_id
                ));
            }
            IndexedFileStreamRead::MissingIndex => {
                return Err(format!(
                    "File('{}') is missing from the file read index; rebuild projections before publishing",
                    request.file_id
                ));
            }
            IndexedFileStreamRead::StaleIndex { content_hash, .. } => {
                return Err(format!(
                    "File('{}') has stale indexed content '{}'; rebuild projections before publishing",
                    request.file_id, content_hash
                ));
            }
        };

        let public_base_url = self
            .secret(tenant, "published_blob_public_base_url")
            .ok_or_else(|| "missing published_blob_public_base_url secret".to_string())?;
        let endpoint = self
            .secret(tenant, "published_blob_endpoint")
            .ok_or_else(|| "missing published_blob_endpoint secret".to_string())?;
        let bucket = self
            .secret(tenant, "published_blob_bucket")
            .unwrap_or_else(|| "published-assets".to_string());
        let prefix = request
            .public_key_prefix
            .as_deref()
            .filter(|prefix| !prefix.trim().is_empty())
            .unwrap_or(DEFAULT_PUBLIC_ASSET_PREFIX);

        let storage_key = public_storage_key(
            prefix,
            &request.owner_entity_type,
            &request.owner_entity_id,
            &request.kind,
            &content_hash,
            &mime_type,
        );
        put_public_blob(
            self,
            tenant,
            endpoint.as_str(),
            bucket.as_str(),
            &storage_key,
            &mime_type,
            &bytes,
        )
        .await?;

        let public_url = format!(
            "{}/{}",
            public_base_url.trim_end_matches('/'),
            storage_key.trim_start_matches('/')
        );
        let asset_id = published_asset_id(
            tenant.as_str(),
            &request.owner_entity_type,
            &request.owner_entity_id,
            &request.kind,
            &content_hash,
        );
        let Some(store) = self.turso_store_for_tenant(tenant.as_str()).await else {
            return Err("published assets require a Turso tenant store".to_string());
        };

        store
            .upsert_published_asset(&PublishedAssetUpsert {
                id: asset_id,
                tenant: tenant.to_string(),
                source_file_id: request.file_id,
                source_file_version_id: request.source_file_version_id,
                content_hash,
                kind: request.kind,
                mime_type,
                byte_length: bytes.len() as i64,
                public_storage_key: storage_key,
                public_url,
                owner_entity_type: request.owner_entity_type,
                owner_entity_id: request.owner_entity_id,
                status: "published".to_string(),
            })
            .await
            .map_err(|e| format!("failed to persist published asset: {e}"))
    }

    fn secret(&self, tenant: &TenantId, key: &str) -> Option<String> {
        self.secrets_vault
            .as_ref()
            .and_then(|vault| vault.get_secret(tenant.as_str(), key))
    }
}

async fn put_public_blob(
    state: &ServerState,
    tenant: &TenantId,
    endpoint: &str,
    bucket: &str,
    storage_key: &str,
    mime_type: &str,
    bytes: &[u8],
) -> Result<(), String> {
    let url = format!(
        "{}/{}/{}",
        endpoint.trim_end_matches('/'),
        bucket.trim_matches('/'),
        storage_key.trim_start_matches('/')
    );
    let mut request = public_blob_http_client()
        .put(&url)
        .body(bytes.to_vec())
        .header(CONTENT_TYPE, stream_content_type(mime_type));
    let headers = build_public_blob_put_headers(state, tenant, &url, mime_type, bytes)?;
    for (header_name, header_value) in &headers {
        request = request.header(header_name, header_value);
    }

    let response = request
        .send()
        .await
        .map_err(|e| format!("public blob PUT request failed for '{storage_key}': {e}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "public blob PUT failed for '{storage_key}' with HTTP {}",
            response.status()
        ));
    }
    Ok(())
}

fn build_public_blob_put_headers(
    state: &ServerState,
    tenant: &TenantId,
    url: &str,
    mime_type: &str,
    bytes: &[u8],
) -> Result<HeaderMap, String> {
    let mut headers = HeaderMap::new();
    let access_key = state
        .secret(tenant, "published_blob_access_key")
        .or_else(|| state.secret(tenant, "blob_access_key"));
    let secret_key = state
        .secret(tenant, "published_blob_secret_key")
        .or_else(|| state.secret(tenant, "blob_secret_key"));
    let (Some(access_key), Some(secret_key)) = (access_key, secret_key) else {
        return Ok(headers);
    };

    let payload_hash = sha256_hex(bytes);
    let datetime = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let date = &datetime[..8];
    let (host, path) = parse_url_host_path(url);
    let canonical_uri = uri_encode_path(path);
    let region = "auto";
    let service = "s3";
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let canonical_headers = format!(
        "content-type:{}\nhost:{host}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{datetime}\n",
        stream_content_type(mime_type)
    );
    let signed_headers = "content-type;host;x-amz-content-sha256;x-amz-date";
    let canonical_request =
        format!("PUT\n{canonical_uri}\n\n{canonical_headers}\n{signed_headers}\n{payload_hash}");
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
            .map_err(|e| format!("invalid public blob authorization header: {e}"))?,
    );
    headers.insert(
        "x-amz-date",
        HeaderValue::from_str(&datetime).map_err(|e| format!("invalid x-amz-date header: {e}"))?,
    );
    headers.insert(
        "x-amz-content-sha256",
        HeaderValue::from_str(&payload_hash)
            .map_err(|e| format!("invalid x-amz-content-sha256 header: {e}"))?,
    );
    headers.insert(
        HOST,
        HeaderValue::from_str(host).map_err(|e| format!("invalid public blob host header: {e}"))?,
    );
    Ok(headers)
}

fn public_storage_key(
    prefix: &str,
    owner_entity_type: &str,
    owner_entity_id: &str,
    kind: &str,
    content_hash: &str,
    mime_type: &str,
) -> String {
    let hash = content_hash.trim_start_matches("sha256:");
    format!(
        "{}/{}/{}/{}-{}.{}",
        sanitize_path_segment(prefix).trim_matches('/'),
        sanitize_path_segment(owner_entity_type),
        sanitize_path_segment(owner_entity_id),
        sanitize_path_segment(kind),
        hash,
        extension_for_mime(mime_type)
    )
}

fn published_asset_id(
    tenant: &str,
    owner_entity_type: &str,
    owner_entity_id: &str,
    kind: &str,
    content_hash: &str,
) -> String {
    let seed = format!("{tenant}\n{owner_entity_type}\n{owner_entity_id}\n{kind}\n{content_hash}");
    let digest = sha256_hex(seed.as_bytes());
    format!("pa-{}", &digest[..32])
}

fn sanitize_path_segment(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '/' | '.') {
            out.push(ch);
        } else {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

fn extension_for_mime(mime_type: &str) -> &'static str {
    match mime_type.split(';').next().unwrap_or("").trim() {
        "text/html" => "html",
        "text/css" => "css",
        "text/javascript" | "application/javascript" => "js",
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        "application/json" => "json",
        "text/markdown" => "md",
        _ => "bin",
    }
}

fn stream_content_type(mime_type: &str) -> String {
    if mime_type.trim().is_empty() {
        "application/octet-stream".to_string()
    } else {
        mime_type.to_string()
    }
}

fn public_blob_http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(reqwest::Client::new)
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
