//! Bounded TypeSafe HTTP adapter for the injected System One evaluation boundary.
//!
//! Only this environment adapter performs HTTP. Dispatch and guard evaluation use
//! [`SystemOneProvider`], which deterministic simulations can implement directly.

use std::io::{self, Write};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::Semaphore;

use crate::secrets::vault::SecretsVault;

/// Tenant-local secret used to authenticate TypeSafe evaluations.
pub const TYPESAFE_SECRET_NAME: &str = "TYPESAFE_API_KEY";

/// Maximum serialized request size sent to TypeSafe.
pub const SYSTEM_ONE_REQUEST_BYTE_BUDGET: usize = 64 * 1024;

/// Maximum response body accepted from TypeSafe.
pub const SYSTEM_ONE_RESPONSE_BYTE_BUDGET: usize = 128 * 1024;

/// Maximum simultaneous HTTP evaluations handled by one provider.
pub const SYSTEM_ONE_CONCURRENCY_BUDGET: usize = 8;

/// Deadline for an evaluation, including connection and response-body reads.
pub const SYSTEM_ONE_EVALUATION_TIMEOUT: Duration = Duration::from_secs(15);

pub(crate) const TYPESAFE_ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// External environment boundary for named, typed System One questions.
///
/// The request follows TypeSafe's `model`, `state`, and `questions` shape. The
/// caller validates the returned answer schema before evaluating any guard.
/// Implementations must keep credentials and customer input out of errors.
#[async_trait]
pub trait SystemOneProvider: Send + Sync {
    /// Evaluate a captured request using credentials belonging to `tenant`.
    async fn evaluate(&self, tenant: &str, request: &Value) -> Result<Value, String>;
}

/// Production TypeSafe adapter with bounded payloads, concurrency, and duration.
///
/// Credentials are read from the tenant vault on every request, so secret
/// rotation and deletion take effect without rebuilding the provider. Shared
/// platform credentials are deliberately excluded. Failed evaluations are not
/// automatically retried; durable attempt handling belongs to dispatch.
pub struct TypesafeSystemOneProvider {
    vault: Arc<SecretsVault>,
    client: reqwest::Client,
    endpoint: String,
    evaluations: Semaphore,
}

impl TypesafeSystemOneProvider {
    /// Construct an adapter for the fixed TypeSafe evaluation endpoint.
    pub fn new(vault: Arc<SecretsVault>) -> Self {
        Self::build(vault, TYPESAFE_ENDPOINT.to_string(), true)
    }

    fn build(vault: Arc<SecretsVault>, endpoint: String, https_only: bool) -> Self {
        // determinism-ok: HTTP and its real deadlines belong to this injected environment adapter.
        let client = reqwest::Client::builder()
            .https_only(https_only)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(SYSTEM_ONE_EVALUATION_TIMEOUT)
            .build()
            .expect("static TypeSafe HTTP client configuration must be valid"); // ci-ok: static builder options
        Self {
            vault,
            client,
            endpoint,
            evaluations: Semaphore::new(SYSTEM_ONE_CONCURRENCY_BUDGET),
        }
    }

    #[cfg(test)]
    fn for_test(vault: Arc<SecretsVault>, origin: String) -> Self {
        Self::build(vault, format!("{origin}/v1/systemone"), false)
    }
}

#[async_trait]
impl SystemOneProvider for TypesafeSystemOneProvider {
    async fn evaluate(&self, tenant: &str, request: &Value) -> Result<Value, String> {
        let _permit = self
            .evaluations
            .try_acquire()
            .map_err(|_| "System One concurrency budget exhausted".to_string())?;
        let key = self
            .vault
            .get_tenant_secret(tenant, TYPESAFE_SECRET_NAME)
            .filter(|key| !key.trim().is_empty())
            .ok_or_else(|| "System One tenant credential is not configured".to_string())?;

        let mut body = BoundedRequestBody::default();
        serde_json::to_writer(&mut body, request)
            .map_err(|_| "System One request byte budget exceeded".to_string())?;

        // determinism-ok: all HTTP execution is behind SystemOneProvider and replaced in simulation.
        let mut response = self
            .client
            .post(&self.endpoint)
            .bearer_auth(&key)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.bytes)
            .send()
            .await
            .map_err(redacted_transport_error)?;
        if !response.status().is_success() {
            // Error bodies may echo supplied state or credentials. Never propagate them.
            return Err(format!(
                "System One provider returned HTTP {}",
                response.status().as_u16()
            ));
        }
        if response
            .content_length()
            .is_some_and(|size| size > SYSTEM_ONE_RESPONSE_BYTE_BUDGET as u64)
        {
            return Err("System One response byte budget exceeded".to_string());
        }

        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(redacted_transport_error)? {
            if chunk.len() > SYSTEM_ONE_RESPONSE_BYTE_BUDGET.saturating_sub(bytes.len()) {
                return Err("System One response byte budget exceeded".to_string());
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes)
            .map_err(|_| "System One provider returned invalid JSON".to_string())
    }
}

fn redacted_transport_error(error: reqwest::Error) -> String {
    if error.is_timeout() {
        "System One evaluation timed out".to_string()
    } else {
        "System One provider transport failed".to_string()
    }
}

#[derive(Default)]
struct BoundedRequestBody {
    bytes: Vec<u8>,
}

impl Write for BoundedRequestBody {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > SYSTEM_ONE_REQUEST_BYTE_BUDGET.saturating_sub(self.bytes.len()) {
            return Err(io::Error::other("request byte budget exceeded"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
#[path = "provider_test.rs"]
mod tests;
