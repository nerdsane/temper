//! Overlay process env into WASM integration config after secret resolution.
//!
//! WASM guests cannot read host environment variables. Named-sandbox connect
//! (`TEMPER_SANDBOX_NAME` / `TEMPER_SANDBOX_URL`) and Datadog credentials
//! therefore have to be copied into `ctx.config` here.
//!
//! Tests inject values. They do not read process env.

use std::collections::BTreeMap;

/// Config key for the named sandbox (expected `dsf` on Deep Sci-Fi).
pub const TEMPER_SANDBOX_NAME_KEY: &str = "temper_sandbox_name";
/// Config key for the named sandbox URL.
pub const TEMPER_SANDBOX_URL_KEY: &str = "temper_sandbox_url";

const SECRET_MARKER: &str = "{secret:";

fn usable(raw: Option<&str>) -> Option<&str> {
    let value = raw?.trim();
    if value.is_empty() || value.contains(SECRET_MARKER) {
        None
    } else {
        Some(value)
    }
}

fn overlay_key(config: &mut BTreeMap<String, String>, key: &str, incoming: Option<&str>) {
    if usable(config.get(key).map(String::as_str)).is_some() {
        return;
    }
    if let Some(value) = usable(incoming) {
        config.insert(key.to_string(), value.to_string());
    }
}

fn overlay_declared(config: &mut BTreeMap<String, String>, key: &str, incoming: Option<&str>) {
    if !config.contains_key(key) {
        return;
    }
    overlay_key(config, key, incoming);
}

/// Overlay named-sandbox values. Inserts keys even when they were absent.
///
/// Already-resolved config values win. Empty and unresolved `{secret:...}`
/// values are replaced.
pub fn overlay_named_sandbox_values(
    config: &mut BTreeMap<String, String>,
    name: Option<&str>,
    url: Option<&str>,
) {
    overlay_key(config, TEMPER_SANDBOX_NAME_KEY, name);
    overlay_key(config, TEMPER_SANDBOX_URL_KEY, url);
}

/// Overlay Datadog values only for keys the trigger already declared.
///
/// Does not insert undeclared keys, so unrelated WASM guests do not receive
/// Datadog credentials. Never logs the values.
pub fn overlay_datadog_values(
    config: &mut BTreeMap<String, String>,
    site: Option<&str>,
    access_token: Option<&str>,
    api_key: Option<&str>,
    app_key: Option<&str>,
) {
    overlay_declared(config, "dd_site", site);
    overlay_declared(config, "dd_access_token", access_token);
    overlay_declared(config, "dd_api_key", api_key);
    overlay_declared(config, "dd_app_key", app_key);
}

/// Copy `TEMPER_SANDBOX_NAME` / `TEMPER_SANDBOX_URL` from the process env.
pub fn overlay_named_sandbox_env(config: &mut BTreeMap<String, String>) {
    let name = std::env::var("TEMPER_SANDBOX_NAME") // determinism-ok: production provision config, not entity state
        .ok();
    let url = std::env::var("TEMPER_SANDBOX_URL") // determinism-ok: production provision config, not entity state
        .ok();
    overlay_named_sandbox_values(config, name.as_deref(), url.as_deref());
}

/// Copy Datadog env names into already-declared trigger config keys.
pub fn overlay_datadog_env(config: &mut BTreeMap<String, String>) {
    let site = std::env::var("DD_SITE") // determinism-ok: production Datadog site, not entity state
        .ok();
    let access_token = std::env::var("DD_ACCESS_TOKEN") // determinism-ok: production Datadog auth, not entity state
        .ok();
    let api_key = std::env::var("DD_API_KEY") // determinism-ok: production Datadog auth, not entity state
        .ok();
    let app_key = std::env::var("DD_APP_KEY") // determinism-ok: production Datadog auth, not entity state
        .ok();
    overlay_datadog_values(
        config,
        site.as_deref(),
        access_token.as_deref(),
        api_key.as_deref(),
        app_key.as_deref(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_sandbox_inserts_when_absent() {
        let mut config = BTreeMap::new();
        overlay_named_sandbox_values(&mut config, Some("dsf"), Some("https://dsf.example"));
        assert_eq!(config[TEMPER_SANDBOX_NAME_KEY], "dsf");
        assert_eq!(config[TEMPER_SANDBOX_URL_KEY], "https://dsf.example");
    }

    #[test]
    fn named_sandbox_replaces_unresolved_secret_templates() {
        let mut config = BTreeMap::new();
        config.insert(
            TEMPER_SANDBOX_NAME_KEY.to_string(),
            "{secret:temper_sandbox_name}".to_string(),
        );
        config.insert(
            TEMPER_SANDBOX_URL_KEY.to_string(),
            "{secret:temper_sandbox_url}".to_string(),
        );
        overlay_named_sandbox_values(&mut config, Some("dsf"), Some("https://dsf.example"));
        assert_eq!(config[TEMPER_SANDBOX_NAME_KEY], "dsf");
        assert_eq!(config[TEMPER_SANDBOX_URL_KEY], "https://dsf.example");
    }

    #[test]
    fn named_sandbox_does_not_overwrite_resolved_values() {
        let mut config = BTreeMap::new();
        config.insert(TEMPER_SANDBOX_NAME_KEY.to_string(), "kept".to_string());
        config.insert(
            TEMPER_SANDBOX_URL_KEY.to_string(),
            "https://kept.example".to_string(),
        );
        overlay_named_sandbox_values(&mut config, Some("dsf"), Some("https://dsf.example"));
        assert_eq!(config[TEMPER_SANDBOX_NAME_KEY], "kept");
        assert_eq!(config[TEMPER_SANDBOX_URL_KEY], "https://kept.example");
    }

    #[test]
    fn named_sandbox_ignores_empty_incoming() {
        let mut config = BTreeMap::new();
        overlay_named_sandbox_values(&mut config, Some(""), Some("   "));
        assert!(config.is_empty());
    }

    #[test]
    fn datadog_overlay_skips_undeclared_keys() {
        let mut config = BTreeMap::new();
        overlay_datadog_values(
            &mut config,
            Some("datadoghq.com"),
            Some("token"),
            Some("api"),
            Some("app"),
        );
        assert!(config.is_empty());
    }

    #[test]
    fn datadog_overlay_fills_declared_unresolved_keys() {
        let mut config = BTreeMap::new();
        config.insert("dd_site".to_string(), "{secret:dd_site}".to_string());
        config.insert("dd_api_key".to_string(), String::new());
        overlay_datadog_values(
            &mut config,
            Some("datadoghq.eu"),
            Some("token-must-not-insert"),
            Some("api-from-env"),
            Some("app-must-not-insert"),
        );
        assert_eq!(config["dd_site"], "datadoghq.eu");
        assert_eq!(config["dd_api_key"], "api-from-env");
        assert!(!config.contains_key("dd_access_token"));
        assert!(!config.contains_key("dd_app_key"));
    }
}
