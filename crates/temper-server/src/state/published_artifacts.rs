use hmac::{Hmac, Mac};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HOST, HeaderMap, HeaderValue};
use sha2::{Digest, Sha256};
use std::sync::OnceLock;
use temper_runtime::tenant::TenantId;
use tracing::instrument;

use crate::storage::{PublishedArtifactStoreRow, PublishedArtifactStoreUpsert};

use super::{IndexedFileStreamRead, ServerState};

const DEFAULT_PUBLIC_ARTIFACT_NAMESPACE: &str = "published-artifacts";

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone)]
pub struct PublishFileArtifactRequest {
    pub file_id: String,
    pub label: String,
    pub owner_ref_type: String,
    pub owner_ref_id: String,
    pub source_file_version_id: String,
    pub namespace: Option<String>,
}

impl ServerState {
    #[instrument(skip_all, fields(
        otel.name = "state.publish_file_artifact",
        tenant = %tenant,
        file_id = %request.file_id,
        artifact_label = %request.label,
        owner_ref_type = %request.owner_ref_type,
        owner_ref_id = %request.owner_ref_id,
    ))]
    pub async fn publish_file_artifact(
        &self,
        tenant: &TenantId,
        request: PublishFileArtifactRequest,
    ) -> Result<PublishedArtifactStoreRow, String> {
        let source_display = if request.source_file_version_id.trim().is_empty() {
            format!("File('{}')", request.file_id)
        } else {
            format!("FileVersion('{}')", request.source_file_version_id)
        };
        let stream = if request.source_file_version_id.trim().is_empty() {
            self.read_file_stream_indexed(tenant, &request.file_id)
                .await?
        } else {
            self.read_file_version_stream_indexed(tenant, &request.source_file_version_id)
                .await?
        };
        let (content_hash, mime_type, bytes) = match stream {
            IndexedFileStreamRead::Content {
                content_hash,
                mime_type,
                bytes,
            } => (content_hash, mime_type, bytes),
            IndexedFileStreamRead::NoContent { .. } => {
                return Err(format!("{source_display} has no content to publish"));
            }
            IndexedFileStreamRead::MissingIndex => {
                return Err(format!(
                    "{source_display} is missing from the file read index; rebuild projections before publishing",
                ));
            }
            IndexedFileStreamRead::StaleIndex { content_hash, .. } => {
                return Err(format!(
                    "{source_display} has stale indexed content '{content_hash}'; rebuild projections before publishing",
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
            .unwrap_or_else(|| DEFAULT_PUBLIC_ARTIFACT_NAMESPACE.to_string());
        let namespace = request
            .namespace
            .as_deref()
            .filter(|namespace| !namespace.trim().is_empty())
            .unwrap_or(DEFAULT_PUBLIC_ARTIFACT_NAMESPACE);

        let storage_key = public_storage_key(
            namespace,
            &request.owner_ref_type,
            &request.owner_ref_id,
            &request.label,
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
        let artifact_id = published_artifact_id(
            tenant.as_str(),
            &request.owner_ref_type,
            &request.owner_ref_id,
            &request.label,
            &content_hash,
        );
        let artifact = PublishedArtifactStoreUpsert {
            id: artifact_id,
            tenant: tenant.to_string(),
            source_file_id: request.file_id,
            source_file_version_id: request.source_file_version_id,
            content_hash,
            label: request.label,
            mime_type,
            byte_length: bytes.len() as i64,
            public_storage_key: storage_key,
            public_url,
            owner_ref_type: request.owner_ref_type,
            owner_ref_id: request.owner_ref_id,
            status: "published".to_string(),
        };

        let store = self
            .metadata_store_for_tenant(tenant.as_str())
            .await
            .ok_or_else(|| "published artifact metadata store unavailable".to_string())?;
        let persisted = store
            .upsert_published_artifact(&artifact)
            .await
            .map_err(|e| format!("failed to persist published artifact: {e}"))?;
        tracing::info!(
            tenant = %tenant,
            artifact_id = %persisted.id,
            owner_ref_type = %persisted.owner_ref_type,
            owner_ref_id = %persisted.owner_ref_id,
            artifact_label = %persisted.label,
            metadata_backend = store.backend_name(),
            "published artifact metadata persisted"
        );
        Ok(persisted)
    }

    fn secret(&self, tenant: &TenantId, key: &str) -> Option<String> {
        self.secrets_vault
            .as_ref()
            .and_then(|vault| vault.get_secret(tenant.as_str(), key))
    }
}

#[instrument(skip_all, fields(
    otel.name = "state.put_public_blob",
    tenant = %tenant,
    bucket = %bucket,
    storage_key = %storage_key,
    endpoint_host = tracing::field::Empty,
    mime_type = %stream_content_type(mime_type),
    byte_length = bytes.len(),
    http.status_code = tracing::field::Empty,
))]
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
    let (endpoint_host, _) = parse_url_host_path(&url);
    tracing::Span::current().record("endpoint_host", endpoint_host);
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
        .map_err(|e| public_blob_put_request_error(endpoint, bucket, storage_key, &e))?;
    let status = response.status();
    tracing::Span::current().record("http.status_code", status.as_u16());
    if !status.is_success() {
        let error = public_blob_put_status_error(bucket, endpoint, storage_key, status);
        tracing::warn!(
            http.status_code = %status,
            endpoint_host,
            bucket,
            storage_key,
            "public blob PUT failed"
        );
        return Err(error);
    }
    tracing::info!(
        http.status_code = %status,
        endpoint_host,
        bucket,
        storage_key,
        "public blob PUT succeeded"
    );
    Ok(())
}

fn public_blob_put_request_error(
    endpoint: &str,
    bucket: &str,
    storage_key: &str,
    error: &reqwest::Error,
) -> String {
    let (endpoint_host, _) = parse_url_host_path(endpoint);
    format!(
        "public blob PUT request failed for bucket '{bucket}' key '{storage_key}' via endpoint host '{endpoint_host}': {error}"
    )
}

fn public_blob_put_status_error(
    bucket: &str,
    endpoint: &str,
    storage_key: &str,
    status: reqwest::StatusCode,
) -> String {
    let (endpoint_host, _) = parse_url_host_path(endpoint);
    format!(
        "public blob PUT failed for bucket '{bucket}' key '{storage_key}' via endpoint host '{endpoint_host}' with HTTP {status}"
    )
}

fn build_public_blob_put_headers(
    state: &ServerState,
    tenant: &TenantId,
    url: &str,
    mime_type: &str,
    bytes: &[u8],
) -> Result<HeaderMap, String> {
    let mut headers = HeaderMap::new();
    if crate::blob_store::is_local_internal_blob_endpoint(url) {
        if let Some(api_key) = std::env::var("TEMPER_API_KEY")
            .ok()
            .filter(|value| !value.trim().is_empty())
        {
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {api_key}"))
                    .map_err(|e| format!("invalid internal blob authorization header: {e}"))?,
            );
        }
        return Ok(headers);
    }

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
    namespace: &str,
    owner_ref_type: &str,
    owner_ref_id: &str,
    label: &str,
    content_hash: &str,
    mime_type: &str,
) -> String {
    let hash = content_hash.trim_start_matches("sha256:");
    format!(
        "{}/{}/{}/{}-{}.{}",
        sanitize_path_segment(namespace).trim_matches('/'),
        sanitize_path_segment(owner_ref_type),
        sanitize_path_segment(owner_ref_id),
        sanitize_path_segment(label),
        hash,
        extension_for_mime(mime_type)
    )
}

fn published_artifact_id(
    tenant: &str,
    owner_ref_type: &str,
    owner_ref_id: &str,
    label: &str,
    content_hash: &str,
) -> String {
    let seed = format!("{tenant}\n{owner_ref_type}\n{owner_ref_id}\n{label}\n{content_hash}");
    let digest = sha256_hex(seed.as_bytes());
    format!("part-{}", &digest[..32])
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

#[cfg(test)]
mod tests;
