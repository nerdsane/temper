//! Encrypted at-rest secret storage per tenant.
//!
//! Uses AES-256-GCM for authenticated encryption with a master key
//! (`TEMPER_VAULT_KEY` env var). Secrets are cached in memory and
//! persisted to Postgres as `(ciphertext, nonce)` pairs.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use aes_gcm::aead::{Aead, OsRng}; // determinism-ok: cryptographic nonce generation, not simulation-visible
use aes_gcm::{AeadCore, Aes256Gcm, Key, KeyInit, Nonce};

/// Maximum number of secrets per tenant (TigerStyle budget).
pub const MAX_SECRETS_PER_TENANT: usize = 100;

/// Maximum secret value size in bytes (TigerStyle budget).
pub const MAX_SECRET_VALUE_BYTES: usize = 8192;

/// Encrypted secret storage with AES-256-GCM.
///
/// Holds a cipher derived from the master key and an in-memory cache
/// of decrypted secrets per tenant. The cache is populated from the
/// persistence layer on startup and kept in sync on writes.
pub struct SecretsVault {
    /// AES-256-GCM cipher instance.
    cipher: Arc<Aes256Gcm>,
    /// In-memory cache: tenant → (key_name → plaintext_value).
    cache: Arc<RwLock<BTreeMap<String, BTreeMap<String, String>>>>,
}

impl SecretsVault {
    /// Create a new vault from a 32-byte master key.
    pub fn new(master_key: &[u8; 32]) -> Self {
        // determinism-ok: cryptographic operations are CPU-bound
        let key = Key::<Aes256Gcm>::from_slice(master_key);
        let cipher = Aes256Gcm::new(key);
        Self {
            cipher: Arc::new(cipher),
            cache: Arc::new(RwLock::new(BTreeMap::new())),
        }
    }

    /// Encrypt a plaintext value, returning `(ciphertext, nonce)`.
    pub fn encrypt(&self, plaintext: &[u8]) -> Result<(Vec<u8>, Vec<u8>), String> {
        debug_assert!(
            plaintext.len() <= MAX_SECRET_VALUE_BYTES,
            "secret value exceeds budget: {} > {}",
            plaintext.len(),
            MAX_SECRET_VALUE_BYTES
        );
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng); // determinism-ok: cryptographic nonce generation
        let ciphertext = self
            .cipher
            .encrypt(&nonce, plaintext)
            .map_err(|e| format!("encryption failed: {e}"))?;
        Ok((ciphertext, nonce.to_vec()))
    }

    /// Decrypt a ciphertext with the given nonce.
    pub fn decrypt(&self, ciphertext: &[u8], nonce_bytes: &[u8]) -> Result<Vec<u8>, String> {
        if nonce_bytes.len() != 12 {
            return Err(format!(
                "invalid nonce length: expected 12, got {}",
                nonce_bytes.len()
            ));
        }
        let nonce = Nonce::from_slice(nonce_bytes);
        self.cipher
            .decrypt(nonce, ciphertext)
            .map_err(|e| format!("decryption failed: {e}"))
    }

    /// Cache a decrypted secret in memory.
    ///
    /// Enforces `MAX_SECRETS_PER_TENANT` budget. Returns `Err` if the
    /// tenant already has the maximum number of secrets and this is a
    /// new key (not an update).
    pub fn cache_secret(&self, tenant: &str, key: &str, value: String) -> Result<(), String> {
        let mut cache = self.cache.write().unwrap(); // ci-ok: infallible lock
        let tenant_secrets = cache.entry(tenant.to_string()).or_default();

        // Budget check: only enforce on new keys, not updates.
        if !tenant_secrets.contains_key(key) && tenant_secrets.len() >= MAX_SECRETS_PER_TENANT {
            return Err(format!(
                "tenant '{tenant}' has reached the maximum of {MAX_SECRETS_PER_TENANT} secrets"
            ));
        }

        tenant_secrets.insert(key.to_string(), value);
        Ok(())
    }

    /// Get a single secret value for a tenant.
    pub fn get_secret(&self, tenant: &str, key: &str) -> Option<String> {
        let cache = self.cache.read().unwrap(); // ci-ok: infallible lock
        cache
            .get(tenant)
            .and_then(|secrets| secrets.get(key).cloned())
    }

    /// Remove a secret from the in-memory cache.
    pub fn remove_secret(&self, tenant: &str, key: &str) -> bool {
        let mut cache = self.cache.write().unwrap(); // ci-ok: infallible lock
        cache
            .get_mut(tenant)
            .map(|secrets| secrets.remove(key).is_some())
            .unwrap_or(false)
    }

    /// List secret key names for a tenant (never values).
    pub fn list_keys(&self, tenant: &str) -> Vec<String> {
        let cache = self.cache.read().unwrap(); // ci-ok: infallible lock
        cache
            .get(tenant)
            .map(|secrets| secrets.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Get all decrypted secrets for a tenant (for WASM host injection).
    pub fn get_tenant_secrets(&self, tenant: &str) -> BTreeMap<String, String> {
        let cache = self.cache.read().unwrap(); // ci-ok: infallible lock
        cache.get(tenant).cloned().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> [u8; 32] {
        [0x42u8; 32]
    }

    #[test]
    fn encrypt_decrypt_round_trip() {
        let vault = SecretsVault::new(&test_key());
        let plaintext = b"super-secret-api-key";
        let (ciphertext, nonce) = vault.encrypt(plaintext).unwrap();
        let decrypted = vault.decrypt(&ciphertext, &nonce).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn different_nonce_per_encryption() {
        let vault = SecretsVault::new(&test_key());
        let (_, nonce1) = vault.encrypt(b"test").unwrap();
        let (_, nonce2) = vault.encrypt(b"test").unwrap();
        assert_ne!(nonce1, nonce2, "each encryption should use a unique nonce");
    }

    #[test]
    fn cache_and_retrieve() {
        let vault = SecretsVault::new(&test_key());
        vault
            .cache_secret("tenant-a", "API_KEY", "sk-123".into())
            .unwrap();
        assert_eq!(
            vault.get_secret("tenant-a", "API_KEY"),
            Some("sk-123".into())
        );
        assert_eq!(vault.get_secret("tenant-a", "MISSING"), None);
        assert_eq!(vault.get_secret("tenant-b", "API_KEY"), None);
    }

    #[test]
    fn budget_enforcement() {
        let vault = SecretsVault::new(&test_key());
        for i in 0..MAX_SECRETS_PER_TENANT {
            vault
                .cache_secret("t", &format!("key-{i}"), "v".into())
                .unwrap();
        }
        // 101st key should fail
        let result = vault.cache_secret("t", "key-overflow", "v".into());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("maximum"));

        // Updating an existing key should still work
        vault.cache_secret("t", "key-0", "updated".into()).unwrap();
    }

    #[test]
    fn remove_secret_works() {
        let vault = SecretsVault::new(&test_key());
        vault.cache_secret("t", "k", "v".into()).unwrap();
        assert!(vault.remove_secret("t", "k"));
        assert!(!vault.remove_secret("t", "k")); // already removed
        assert_eq!(vault.get_secret("t", "k"), None);
    }

    #[test]
    fn list_keys_returns_names_only() {
        let vault = SecretsVault::new(&test_key());
        vault.cache_secret("t", "B_KEY", "val-b".into()).unwrap();
        vault.cache_secret("t", "A_KEY", "val-a".into()).unwrap();
        let keys = vault.list_keys("t");
        assert_eq!(keys, vec!["A_KEY", "B_KEY"]); // BTreeMap order
    }

    #[test]
    fn get_tenant_secrets_for_wasm() {
        let vault = SecretsVault::new(&test_key());
        vault.cache_secret("t", "K1", "V1".into()).unwrap();
        vault.cache_secret("t", "K2", "V2".into()).unwrap();
        let secrets = vault.get_tenant_secrets("t");
        assert_eq!(secrets.len(), 2);
        assert_eq!(secrets["K1"], "V1");
        assert_eq!(secrets["K2"], "V2");
    }

    #[test]
    fn invalid_nonce_length_fails() {
        let vault = SecretsVault::new(&test_key());
        let result = vault.decrypt(b"ciphertext", b"short");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("invalid nonce length"));
    }
}
