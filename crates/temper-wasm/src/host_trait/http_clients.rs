//! Bounded, per-engine reuse of production HTTP clients.
//!
//! Loading the TLS trust store for every WASM callback is expensive. Only the
//! transport configuration is shared: host secrets, capabilities, streams and
//! request headers remain per invocation. Never put this cache in the
//! process-global test compiler: connection pools belong to their Tokio runtime.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

const CLIENT_CONFIG_BUDGET: usize = 64;

#[derive(Clone, PartialEq, Eq)]
struct ClientConfig {
    timeout: Duration,
    ca_certs: Vec<(String, String)>,
}

impl ClientConfig {
    fn new(secrets: &BTreeMap<String, String>, timeout: Duration) -> Self {
        Self {
            timeout,
            ca_certs: secrets
                .iter()
                .filter(|(key, _)| key.starts_with("ca_cert:"))
                .map(|(key, pem)| (key.clone(), pem.clone()))
                .collect(),
        }
    }
}

pub(super) struct HttpClients {
    pub(super) client: reqwest::Client,
    pub(super) internal_client: reqwest::Client,
}

impl HttpClients {
    pub(super) fn build(secrets: &BTreeMap<String, String>, timeout: Duration) -> Self {
        Self::from_config(&ClientConfig::new(secrets, timeout))
    }

    fn from_config(config: &ClientConfig) -> Self {
        Self {
            client: build_client(config, false),
            internal_client: build_client(config, true),
        }
    }
}

struct Entry {
    config: ClientConfig,
    clients: Arc<OnceLock<Arc<HttpClients>>>,
}

/// Per-engine HTTP connection pools, separated by timeout and private CA set.
///
/// The 64-entry least-recently-used cache contains no invocation credentials or
/// mutable guest state. Concurrent misses share initialization for a retained
/// configuration without holding the cache lock while loading TLS roots.
pub struct HttpClientCache {
    budget: usize,
    entries: Mutex<VecDeque<Entry>>,
}

impl Default for HttpClientCache {
    fn default() -> Self {
        Self::with_budget(CLIENT_CONFIG_BUDGET)
    }
}

impl std::fmt::Debug for HttpClientCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpClientCache")
            .field("configs", &self.len())
            .finish()
    }
}

impl HttpClientCache {
    fn with_budget(budget: usize) -> Self {
        assert!(budget > 0, "HTTP client cache needs a positive capacity");
        Self {
            budget,
            entries: Mutex::new(VecDeque::new()),
        }
    }

    pub(super) fn get(
        &self,
        secrets: &BTreeMap<String, String>,
        timeout: Duration,
    ) -> Arc<HttpClients> {
        let config = ClientConfig::new(secrets, timeout);
        let clients = {
            let mut entries = self.lock();
            let entry = if let Some(index) = entries.iter().position(|entry| entry.config == config)
            {
                entries
                    .remove(index)
                    .expect("entry index came from this cache")
            } else {
                if entries.len() >= self.budget {
                    entries.pop_front();
                }
                Entry {
                    config: config.clone(),
                    clients: Arc::new(OnceLock::new()),
                }
            };
            let clients = Arc::clone(&entry.clients);
            entries.push_back(entry);
            clients
        };
        // Initialize outside the map lock. Unrelated hits and cold configurations
        // can proceed; callers of this same retained entry initialize it once.
        Arc::clone(clients.get_or_init(|| Arc::new(HttpClients::from_config(&config))))
    }

    /// Number of retained configurations, including any being initialized.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether no configuration has been requested.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<Entry>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn build_client(config: &ClientConfig, disable_redirects: bool) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(config.timeout);
    if disable_redirects {
        builder = builder
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy();
    }
    for (key, pem) in &config.ca_certs {
        match reqwest::Certificate::from_pem(pem.as_bytes()) {
            Ok(cert) => builder = builder.add_root_certificate(cert),
            Err(error) => {
                tracing::warn!(key, error = %error, "failed to parse CA certificate from secret");
            }
        }
    }
    builder
        .build()
        .expect("production HTTP client configuration must be valid")
}

#[cfg(test)]
#[path = "http_clients_test.rs"]
mod tests;
