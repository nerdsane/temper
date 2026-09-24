//! Host-local byte import through the existing governed OData stream endpoint.
use crate::{
    elicit::{DeniedDecision, denial_from_dispatch_value},
    runtime::RuntimeContext,
};
use anyhow::{Context, Result, bail};
use reqwest::{Client, Url, header::HeaderValue};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fs::OpenOptions, io::Read, path::Path, time::Duration};

const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Upload {
    tenant: String,
    file_id: String,
    local_path: String,
    content_type: String,
    expected_sha256: String,
}
impl Upload {
    fn validate(&self) -> Result<()> {
        if self.tenant.is_empty() || self.tenant.len() > 128 {
            bail!("tenant must contain 1 to 128 bytes");
        }
        HeaderValue::from_str(&self.tenant).context("Invalid tenant header")?;
        if self.file_id.is_empty()
            || self.file_id.len() > 256
            || !self
                .file_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            bail!("file_id must contain only ASCII letters, digits, hyphens or underscores");
        }
        if self.content_type.is_empty() || self.content_type.len() > 256 {
            bail!("content_type must contain 1 to 256 bytes");
        }
        HeaderValue::from_str(&self.content_type).context("Invalid content_type header")?;
        if self.expected_sha256.len() != 64
            || !self
                .expected_sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            bail!("expected_sha256 must be a lowercase SHA-256 digest");
        }
        if !Path::new(&self.local_path).is_absolute() {
            bail!("local_path must be absolute on the MCP host");
        }
        Ok(())
    }
}
fn read_bounded(reader: impl Read, expected: &str) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("Cannot read local upload file")?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        bail!("Upload file exceeded 32 MiB while reading");
    }
    if format!("{:x}", Sha256::digest(&bytes)) != expected {
        bail!("Local upload bytes do not match expected_sha256");
    }
    Ok(bytes)
}
fn read_bytes(path: &str, expected: &str) -> Result<Vec<u8>> {
    let mut options = OpenOptions::new();
    options.read(true);
    // Reject FIFOs without blocking on open, then inspect the opened descriptor.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options
        .open(path)
        .context("Cannot open local upload file")?;
    let metadata = file
        .metadata()
        .context("Cannot inspect local upload file")?;
    if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
        bail!("Upload requires a regular file of at most 32 MiB");
    }
    // One bounded snapshot is both hashed and sent; never reopen after hashing.
    read_bounded(file, expected)
}
struct Uploaded {
    status: reqwest::StatusCode,
    body: String,
    receipt: Value,
}
async fn perform(ctx: &RuntimeContext, upload: &Upload) -> Result<Uploaded> {
    upload.validate()?;
    let path = upload.local_path.clone();
    let expected = upload.expected_sha256.clone();
    let bytes = tokio::task::spawn_blocking(move || read_bytes(&path, &expected))
        .await
        .context("Local upload reader failed")??;
    let size = bytes.len();
    let mut url = Url::parse(&ctx.base_url).context("Invalid configured Temper URL")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        bail!(
            "Configured Temper URL must be an HTTP base URL without credentials, query or fragment"
        );
    }
    url.path_segments_mut()
        .map_err(|()| anyhow::anyhow!("Invalid Temper base URL"))?
        .pop_if_empty()
        .push("tdata")
        .push(&format!("Files('{}')", upload.file_id))
        .push("$value");
    // Never follow redirects carrying credentials or local bytes.
    let http = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(90))
        .build()?;
    let authenticated = |request: reqwest::RequestBuilder| {
        let mut request = request.header("X-Tenant-Id", &upload.tenant);
        if let Some(key) = &ctx.api_key {
            request = request.bearer_auth(key);
        }
        if let Some(session) = &ctx.session_id {
            request = request.header("X-Session-Id", session);
        }
        request
    };
    // Stream PUT can create a missing File. Require an existing readable File first.
    // File has no delete/tombstone transition, so an existing ID cannot become new.
    let mut entity_url = url.clone();
    entity_url
        .path_segments_mut()
        .map_err(|()| anyhow::anyhow!("Invalid File URL"))?
        .pop();
    let existing = authenticated(http.get(entity_url))
        .send()
        .await
        .context("Cannot verify existing File")?;
    if !existing.status().is_success() {
        return read_response(existing, Value::Null).await;
    }
    let response = authenticated(http.put(url))
        .header("Content-Type", &upload.content_type)
        .body(bytes)
        .send()
        .await
        .context("File byte upload failed; inspect File state before retrying")?;
    let receipt = json!({"file_id":upload.file_id,"tenant":upload.tenant,"size_bytes":size,"sha256":upload.expected_sha256,"content_type":upload.content_type,"uploaded":true,"locked":false,"verification":"hash of exact bytes sent; read back File and bytes before Lock"});
    read_response(response, receipt).await
}
async fn read_response(mut response: reqwest::Response, receipt: Value) -> Result<Uploaded> {
    let status = response.status();
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .context("Cannot read upload response")?
    {
        if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
            body.extend_from_slice(&chunk[..MAX_RESPONSE_BYTES - body.len()]);
            return Ok(Uploaded {
                status,
                body: format!(
                    "{} [response truncated at 64 KiB]",
                    String::from_utf8_lossy(&body)
                ),
                receipt: Value::Null,
            });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Uploaded {
        status,
        body: String::from_utf8_lossy(&body).into_owned(),
        receipt,
    })
}
/// Import bytes only: never create, Lock, or attest an artifact.
pub(crate) async fn upload_file_bytes(
    ctx: &RuntimeContext,
    args: &Value,
) -> (Result<String>, Vec<DeniedDecision>) {
    let upload: Upload = match serde_json::from_value(args.clone()) {
        Ok(value) => value,
        Err(error) => return (Err(error.into()), Vec::new()),
    };
    match perform(ctx, &upload).await {
        Err(error) => (Err(error), Vec::new()),
        Ok(response) if response.status.is_success() && !response.receipt.is_null() => {
            (Ok(response.receipt.to_string()), Vec::new())
        }
        Ok(response) => {
            let denials = temper_sandbox::helpers::format_authz_denied(&response.body)
                .or_else(|| serde_json::from_str::<Value>(&response.body).ok())
                .and_then(|v| denial_from_dispatch_value(&upload.tenant, &v))
                .into_iter()
                .collect();
            (
                Err(anyhow::anyhow!(
                    "File byte upload HTTP {}: {}",
                    response.status.as_u16(),
                    response.body
                )),
                denials,
            )
        }
    }
}
#[cfg(test)]
#[path = "file_bytes_test.rs"]
mod tests;
