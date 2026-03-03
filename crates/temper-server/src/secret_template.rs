//! Secret template resolution for integration configs.
//!
//! Scans `BTreeMap<String, String>` values for `{secret:KEY}` patterns
//! and substitutes them with values from the [`SecretsVault`]. Missing
//! secrets leave the template pattern as-is so callers can detect
//! unresolved references without crashing.

use std::collections::BTreeMap;

use crate::secrets_vault::SecretsVault;

/// Resolve `{secret:KEY}` patterns in integration config values.
///
/// For each value in `config`, every occurrence of `{secret:KEY}` is
/// replaced with the corresponding secret from `vault` for the given
/// `tenant`. If a secret is not found, the pattern is left verbatim.
///
/// Multiple templates in a single value are supported.
pub fn resolve_secret_templates(
    config: &BTreeMap<String, String>,
    vault: &SecretsVault,
    tenant: &str,
) -> BTreeMap<String, String> {
    config
        .iter()
        .map(|(k, v)| (k.clone(), resolve_value(v, vault, tenant)))
        .collect()
}

/// Resolve all `{secret:...}` patterns within a single string value.
fn resolve_value(value: &str, vault: &SecretsVault, tenant: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut remaining = value;

    while let Some(start) = remaining.find("{secret:") {
        // Push everything before the pattern.
        result.push_str(&remaining[..start]);

        let after_prefix = &remaining[start + 8..]; // skip "{secret:"
        if let Some(end) = after_prefix.find('}') {
            let key = &after_prefix[..end];
            if let Some(secret) = vault.get_secret(tenant, key) {
                result.push_str(&secret);
            } else {
                // Missing secret — leave the pattern as-is.
                result.push_str(&remaining[start..start + 8 + end + 1]);
            }
            remaining = &after_prefix[end + 1..];
        } else {
            // No closing brace — treat as literal text.
            result.push_str(&remaining[start..]);
            remaining = "";
        }
    }

    // Push any trailing text after the last pattern.
    result.push_str(remaining);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_vault() -> SecretsVault {
        let vault = SecretsVault::new(&[0x42u8; 32]);
        vault
            .cache_secret("t1", "API_KEY", "sk-live-abc123".into())
            .unwrap();
        vault
            .cache_secret("t1", "DB_PASS", "hunter2".into())
            .unwrap();
        vault
    }

    #[test]
    fn template_with_valid_secret_resolves() {
        let vault = test_vault();
        let mut config = BTreeMap::new();
        config.insert("auth".into(), "Bearer {secret:API_KEY}".into());

        let resolved = resolve_secret_templates(&config, &vault, "t1");
        assert_eq!(resolved["auth"], "Bearer sk-live-abc123");
    }

    #[test]
    fn missing_secret_left_as_is() {
        let vault = test_vault();
        let mut config = BTreeMap::new();
        config.insert("token".into(), "{secret:MISSING_KEY}".into());

        let resolved = resolve_secret_templates(&config, &vault, "t1");
        assert_eq!(resolved["token"], "{secret:MISSING_KEY}");
    }

    #[test]
    fn multiple_templates_in_one_value() {
        let vault = test_vault();
        let mut config = BTreeMap::new();
        config.insert(
            "dsn".into(),
            "user:{secret:API_KEY}@host/{secret:DB_PASS}".into(),
        );

        let resolved = resolve_secret_templates(&config, &vault, "t1");
        assert_eq!(resolved["dsn"], "user:sk-live-abc123@host/hunter2");
    }

    #[test]
    fn no_templates_passthrough() {
        let vault = test_vault();
        let mut config = BTreeMap::new();
        config.insert("url".into(), "https://example.com".into());
        config.insert("method".into(), "POST".into());

        let resolved = resolve_secret_templates(&config, &vault, "t1");
        assert_eq!(resolved["url"], "https://example.com");
        assert_eq!(resolved["method"], "POST");
    }

    #[test]
    fn nested_and_malformed_patterns_dont_panic() {
        let vault = test_vault();
        let mut config = BTreeMap::new();
        // Unclosed brace
        config.insert("a".into(), "{secret:NO_CLOSE".into());
        // Nested (inner just treated as key chars)
        config.insert("b".into(), "{secret:{secret:API_KEY}}".into());
        // Empty key
        config.insert("c".into(), "{secret:}".into());
        // Just the prefix
        config.insert("d".into(), "{secret:".into());

        let resolved = resolve_secret_templates(&config, &vault, "t1");
        // Unclosed: left as literal
        assert_eq!(resolved["a"], "{secret:NO_CLOSE");
        // Nested: first match is key "{secret:API_KEY", which won't exist
        assert_eq!(resolved["b"], "{secret:{secret:API_KEY}}");
        // Empty key: no secret with empty name → left as-is
        assert_eq!(resolved["c"], "{secret:}");
        // Just prefix, no close: left as literal
        assert_eq!(resolved["d"], "{secret:");
    }

    #[test]
    fn wrong_tenant_does_not_resolve() {
        let vault = test_vault();
        let mut config = BTreeMap::new();
        config.insert("key".into(), "{secret:API_KEY}".into());

        let resolved = resolve_secret_templates(&config, &vault, "other-tenant");
        assert_eq!(resolved["key"], "{secret:API_KEY}");
    }
}
