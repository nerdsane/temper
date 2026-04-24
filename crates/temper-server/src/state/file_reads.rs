use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock};

use futures_util::stream::{self, StreamExt};
use hmac::{Hmac, Mac};
use reqwest::header::{AUTHORIZATION, HOST, HeaderMap, HeaderValue};
use sha2::{Digest, Sha256};
use temper_runtime::tenant::TenantId;
use temper_store_turso::store::field_index::ProjectedEntityFieldsRow;
use tracing::instrument;

use super::ServerState;

const DEFAULT_BLOB_BUCKET: &str = "temper-fs";
const FILE_BATCH_READ_CONCURRENCY: usize = 8;

type HmacSha256 = Hmac<Sha256>;

fn is_local_internal_blob_endpoint(endpoint: &str) -> bool {
    let normalized = endpoint.trim_end_matches('/');
    (normalized.starts_with("http://127.0.0.1:") || normalized.starts_with("http://localhost:"))
        && normalized.contains("/_internal/blobs")
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TextFileReadResult {
    pub file_id: String,
    pub found: bool,
    pub content_hash: String,
    pub mime_type: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TextFileVersionReadResult {
    pub file_version_id: String,
    pub found: bool,
    pub content_hash: String,
    pub mime_type: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileProjectionMeta {
    content_hash: String,
    mime_type: String,
    has_content: bool,
}

impl ServerState {
    #[instrument(skip_all, fields(
        otel.name = "state.read_file_texts_batch",
        tenant = %tenant,
        file_count = file_ids.len(),
    ))]
    pub async fn read_file_texts_batch(
        &self,
        tenant: &TenantId,
        file_ids: &[String],
    ) -> Result<Vec<TextFileReadResult>, String> {
        if file_ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut unique_ids = Vec::new();
        let mut seen = BTreeSet::new();
        for file_id in file_ids {
            if seen.insert(file_id.clone()) {
                unique_ids.push(file_id.clone());
            }
        }

        let meta_by_id = self
            .load_file_projection_metadata_batch(tenant, &unique_ids)
            .await?;

        let meta_by_id = Arc::new(meta_by_id);
        let results = stream::iter(file_ids.iter().cloned())
            .map(|file_id| {
                let meta_by_id = Arc::clone(&meta_by_id);
                async move {
                    let meta = meta_by_id.get(&file_id).cloned();
                    self.read_single_file_text(tenant, file_id, meta).await
                }
            })
            .buffered(FILE_BATCH_READ_CONCURRENCY)
            .collect::<Vec<_>>()
            .await;

        let mut out = Vec::with_capacity(results.len());
        for result in results {
            out.push(result?);
        }
        Ok(out)
    }

    #[instrument(skip_all, fields(
        otel.name = "state.read_file_version_texts_batch",
        tenant = %tenant,
        file_version_count = file_version_ids.len(),
    ))]
    pub async fn read_file_version_texts_batch(
        &self,
        tenant: &TenantId,
        file_version_ids: &[String],
    ) -> Result<Vec<TextFileVersionReadResult>, String> {
        if file_version_ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut unique_ids = Vec::new();
        let mut seen = BTreeSet::new();
        for file_version_id in file_version_ids {
            if seen.insert(file_version_id.clone()) {
                unique_ids.push(file_version_id.clone());
            }
        }

        let meta_by_id = self
            .load_file_version_projection_metadata_batch(tenant, &unique_ids)
            .await?;

        let meta_by_id = Arc::new(meta_by_id);
        let results = stream::iter(file_version_ids.iter().cloned())
            .map(|file_version_id| {
                let meta_by_id = Arc::clone(&meta_by_id);
                async move {
                    let meta = meta_by_id.get(&file_version_id).cloned();
                    self.read_single_file_version_text(tenant, file_version_id, meta)
                        .await
                }
            })
            .buffered(FILE_BATCH_READ_CONCURRENCY)
            .collect::<Vec<_>>()
            .await;

        let mut out = Vec::with_capacity(results.len());
        for result in results {
            out.push(result?);
        }
        Ok(out)
    }

    async fn load_file_projection_metadata_batch(
        &self,
        tenant: &TenantId,
        file_ids: &[String],
    ) -> Result<BTreeMap<String, FileProjectionMeta>, String> {
        let mut by_id = BTreeMap::new();

        if let Some(turso) = self.persistent_store_for_tenant(tenant.as_str()).await {
            let rows = turso
                .load_query_projection_fields_many(
                    tenant.as_str(),
                    "File",
                    file_ids,
                    &["content_hash", "mime_type", "has_content"],
                )
                .await
                .map_err(|e| format!("failed to load File projections: {e}"))?;
            for row in rows {
                by_id.insert(row.entity_id.clone(), file_projection_from_row(row));
            }
        }

        let missing_ids: Vec<String> = file_ids
            .iter()
            .filter(|file_id| !by_id.contains_key(*file_id))
            .cloned()
            .collect();

        for file_id in missing_ids {
            if let Ok(resp) = self.get_tenant_entity_state(tenant, "File", &file_id).await {
                by_id.insert(file_id, file_projection_from_state(&resp.state.fields));
            }
        }

        Ok(by_id)
    }

    async fn load_file_version_projection_metadata_batch(
        &self,
        tenant: &TenantId,
        file_version_ids: &[String],
    ) -> Result<BTreeMap<String, FileProjectionMeta>, String> {
        let mut by_id = BTreeMap::new();

        if let Some(turso) = self.persistent_store_for_tenant(tenant.as_str()).await {
            let rows = turso
                .load_query_projection_fields_many(
                    tenant.as_str(),
                    "FileVersion",
                    file_version_ids,
                    &["content_hash", "mime_type"],
                )
                .await
                .map_err(|e| format!("failed to load FileVersion projections: {e}"))?;
            for row in rows {
                by_id.insert(row.entity_id.clone(), file_version_projection_from_row(row));
            }
        }

        let missing_ids: Vec<String> = file_version_ids
            .iter()
            .filter(|file_version_id| !by_id.contains_key(*file_version_id))
            .cloned()
            .collect();

        for file_version_id in missing_ids {
            if let Ok(resp) = self
                .get_tenant_entity_state(tenant, "FileVersion", &file_version_id)
                .await
            {
                by_id.insert(
                    file_version_id,
                    file_version_projection_from_state(&resp.state.fields),
                );
            }
        }

        Ok(by_id)
    }

    async fn read_single_file_text(
        &self,
        tenant: &TenantId,
        file_id: String,
        meta: Option<FileProjectionMeta>,
    ) -> Result<TextFileReadResult, String> {
        let Some(meta) = meta else {
            return Ok(TextFileReadResult {
                file_id,
                found: false,
                content_hash: String::new(),
                mime_type: String::new(),
                text: String::new(),
            });
        };

        if !meta.has_content || meta.content_hash.is_empty() {
            return Ok(TextFileReadResult {
                file_id,
                found: true,
                content_hash: meta.content_hash,
                mime_type: meta.mime_type,
                text: String::new(),
            });
        }

        let text = self
            .fetch_blob_text_for_hash(tenant, &meta.content_hash)
            .await?
            .unwrap_or_default();

        Ok(TextFileReadResult {
            file_id,
            found: true,
            content_hash: meta.content_hash,
            mime_type: meta.mime_type,
            text,
        })
    }

    async fn read_single_file_version_text(
        &self,
        tenant: &TenantId,
        file_version_id: String,
        meta: Option<FileProjectionMeta>,
    ) -> Result<TextFileVersionReadResult, String> {
        let Some(meta) = meta else {
            return Ok(TextFileVersionReadResult {
                file_version_id,
                found: false,
                content_hash: String::new(),
                mime_type: String::new(),
                text: String::new(),
            });
        };

        if !meta.has_content || meta.content_hash.is_empty() {
            return Ok(TextFileVersionReadResult {
                file_version_id,
                found: true,
                content_hash: meta.content_hash,
                mime_type: meta.mime_type,
                text: String::new(),
            });
        }

        let text = self
            .fetch_blob_text_for_hash(tenant, &meta.content_hash)
            .await?
            .unwrap_or_default();

        Ok(TextFileVersionReadResult {
            file_version_id,
            found: true,
            content_hash: meta.content_hash,
            mime_type: meta.mime_type,
            text,
        })
    }

    async fn fetch_blob_text_for_hash(
        &self,
        tenant: &TenantId,
        content_hash: &str,
    ) -> Result<Option<String>, String> {
        let blob_endpoint = self
            .secrets_vault
            .as_ref()
            .and_then(|vault| vault.get_secret(tenant.as_str(), "blob_endpoint"));

        let blob_bytes = if let Some(endpoint) = blob_endpoint {
            if is_local_internal_blob_endpoint(endpoint.as_str())
                && let Some(store) = self.persistent_store_for_tenant(tenant.as_str()).await
            {
                crate::blobs::get_blob_bytes(
                    &store,
                    &format!("{DEFAULT_BLOB_BUCKET}/{content_hash}"),
                )
                .await
                .map_err(|e| format!("failed to read local blob '{content_hash}': {e}"))?
            } else {
                let bucket = self
                    .secrets_vault
                    .as_ref()
                    .and_then(|vault| vault.get_secret(tenant.as_str(), "blob_bucket"))
                    .unwrap_or_else(|| DEFAULT_BLOB_BUCKET.to_string());
                fetch_external_blob_bytes(
                    self,
                    tenant,
                    endpoint.as_str(),
                    content_hash,
                    bucket.as_str(),
                )
                .await?
            }
        } else if let Some(store) = self.persistent_store_for_tenant(tenant.as_str()).await {
            crate::blobs::get_blob_bytes(&store, &format!("{DEFAULT_BLOB_BUCKET}/{content_hash}"))
                .await
                .map_err(|e| format!("failed to read local blob '{content_hash}': {e}"))?
        } else {
            None
        };

        Ok(blob_bytes.map(|bytes| String::from_utf8_lossy(&bytes).to_string()))
    }
}

fn file_projection_from_row(row: ProjectedEntityFieldsRow) -> FileProjectionMeta {
    FileProjectionMeta {
        content_hash: row
            .fields
            .get("content_hash")
            .cloned()
            .flatten()
            .unwrap_or_default(),
        mime_type: row
            .fields
            .get("mime_type")
            .cloned()
            .flatten()
            .unwrap_or_default(),
        has_content: row
            .fields
            .get("has_content")
            .and_then(|value| value.as_deref())
            .is_some_and(|value| value.eq_ignore_ascii_case("true")),
    }
}

fn file_projection_from_state(fields: &serde_json::Value) -> FileProjectionMeta {
    FileProjectionMeta {
        content_hash: fields
            .get("content_hash")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string(),
        mime_type: fields
            .get("mime_type")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string(),
        has_content: fields
            .get("has_content")
            .and_then(|value| value.as_bool())
            .unwrap_or(false),
    }
}

fn file_version_projection_from_row(row: ProjectedEntityFieldsRow) -> FileProjectionMeta {
    let content_hash = row
        .fields
        .get("content_hash")
        .cloned()
        .flatten()
        .unwrap_or_default();
    FileProjectionMeta {
        has_content: !content_hash.is_empty(),
        content_hash,
        mime_type: row
            .fields
            .get("mime_type")
            .cloned()
            .flatten()
            .unwrap_or_default(),
    }
}

fn file_version_projection_from_state(fields: &serde_json::Value) -> FileProjectionMeta {
    let content_hash = fields
        .get("content_hash")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string();
    FileProjectionMeta {
        has_content: !content_hash.is_empty(),
        content_hash,
        mime_type: fields
            .get("mime_type")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string(),
    }
}

async fn fetch_external_blob_bytes(
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
