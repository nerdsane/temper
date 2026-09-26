//! Reuse of the production host's outbound HTTP clients across invocations.
//!
//! Building a `reqwest::Client` loads and parses the system trust store, which
//! costs tens of milliseconds and takes process-wide OpenSSL locks. A
//! [`ProductionWasmHost`](super::ProductionWasmHost) is built per invocation, so
//! building its clients there charged that cost to every callback and serialized
//! concurrent invocations on the OpenSSL locks. A client is cheap to clone (its
//! connection pool is shared behind an `Arc`), so the cache builds one client
//! pair per distinct configuration and hands out clones.
//!
//! The cache is owned by a [`WasmEngine`](crate::WasmEngine), not a process
//! global: pooled connections belong to the tokio runtime that opened them, and
//! an engine lives inside one server (one runtime), while a process can hold
//! several servers — every `#[tokio::test]` builds its own.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

/// Distinct client configurations retained at once.
///
/// A configuration is a (timeout, private CA set) pair, so the live count is
/// the number of distinct integration timeouts times the tenants with private
/// CAs. Exceeding the budget evicts an entry, which only costs a rebuild on the
/// next use of that configuration.
const CLIENT_CONFIG_BUDGET: usize = 64;

/// Everything that makes two production client pairs differ.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ClientConfig {
    timeout: Duration,
    /// `ca_cert:*` secrets in key order; no other secret affects the clients.
    ca_certs: Vec<(String, String)>,
}

impl ClientConfig {
    fn new(secrets: &BTreeMap<String, String>, timeout: Duration) -> Self {
        let ca_certs = secrets
            .iter()
            .filter(|(key, _)| key.starts_with("ca_cert:"))
            .map(|(key, pem)| (key.clone(), pem.clone()))
            .collect();
        Self { timeout, ca_certs }
    }
}

/// The two clients a production host uses.
#[derive(Clone)]
pub(super) struct HttpClients {
    /// Follows redirects and honours proxy settings.
    pub(super) client: reqwest::Client,
    /// No redirects and no proxy, for internal capability requests.
    pub(super) internal_client: reqwest::Client,
}

impl HttpClients {
    /// Build a fresh client pair for these secrets and timeout.
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

/// Client pairs keyed by configuration, shared by every host an engine serves.
pub struct HttpClientCache {
    budget: usize,
    clients: Mutex<BTreeMap<ClientConfig, HttpClients>>,
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
        assert!(budget > 0, "http client cache budget must be positive");
        Self {
            budget,
            clients: Mutex::new(BTreeMap::new()),
        }
    }

    /// Return the client pair for these secrets and timeout, building it once.
    pub(super) fn get(&self, secrets: &BTreeMap<String, String>, timeout: Duration) -> HttpClients {
        let config = ClientConfig::new(secrets, timeout);
        if let Some(clients) = self.lock().get(&config) {
            return clients.clone();
        }
        // Build outside the lock: a build takes tens of milliseconds and must not
        // stall hosts whose configuration is already cached. Two concurrent
        // misses on one configuration both build; the second insert wins.
        let clients = HttpClients::from_config(&config);
        let mut cached = self.lock();
        if cached.len() >= self.budget && !cached.contains_key(&config) {
            tracing::debug!(
                budget = self.budget,
                "wasm http client cache at budget; evicting one configuration"
            );
            cached.pop_first();
        }
        cached.insert(config, clients.clone());
        clients
    }

    /// Number of distinct configurations currently cached.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether no configuration is cached.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<ClientConfig, HttpClients>> {
        // Entries are inserted whole, so a panic elsewhere cannot leave one torn.
        self.clients.lock().unwrap_or_else(PoisonError::into_inner)
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
            Ok(cert) => {
                builder = builder.add_root_certificate(cert);
            }
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
