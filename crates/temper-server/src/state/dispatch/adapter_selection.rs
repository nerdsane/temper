//! Authorization for native adapter selection (ARN-228, ADR-0161).
//!
//! `type = "adapter"` integrations run in native Rust outside the WASM sandbox.
//! Which adapter runs may be switched at runtime via a mutable entity field, but
//! only within the set the integration's spec explicitly declared — a mutable
//! field must never be able to escalate a benign integration onto an undeclared,
//! unsandboxed host-process adapter.

use std::collections::{BTreeMap, BTreeSet};

/// Why an adapter could not be selected for an integration.
pub(super) enum AdapterSelectionError {
    /// The integration declares no adapter at all (spec misconfiguration).
    Undeclared,
    /// The entity requested an adapter outside the integration's declared set.
    NotPermitted {
        requested: String,
        permitted: Vec<String>,
    },
}

/// The set of adapters an integration's spec permits: the primary `adapter` /
/// `adapter_type`, plus any additional `allowed_adapters` (comma/space list).
fn permitted_adapter_set(config: &BTreeMap<String, String>) -> BTreeSet<String> {
    let mut set = BTreeSet::new();
    for key in ["adapter", "adapter_type"] {
        if let Some(value) = config.get(key).map(|s| s.trim()).filter(|s| !s.is_empty()) {
            set.insert(value.to_string());
        }
    }
    if let Some(list) = config.get("allowed_adapters") {
        for entry in list.split([',', ' ', '\t', '\n']) {
            let entry = entry.trim();
            if !entry.is_empty() {
                set.insert(entry.to_string());
            }
        }
    }
    set
}

/// Resolve which native adapter an integration should run.
///
/// An entity-provided `adapter_type` may switch adapters, but only to one the
/// integration's spec declared (`permitted_adapter_set`). With no override, the
/// declared primary adapter is used. Any override outside the declared set is
/// rejected so a mutable entity field cannot escalate onto an undeclared,
/// unsandboxed adapter (ARN-228).
pub(super) fn select_permitted_adapter(
    entity_adapter_type: Option<&str>,
    config: &BTreeMap<String, String>,
) -> Result<String, AdapterSelectionError> {
    let permitted = permitted_adapter_set(config);
    if permitted.is_empty() {
        return Err(AdapterSelectionError::Undeclared);
    }

    match entity_adapter_type.map(str::trim).filter(|s| !s.is_empty()) {
        Some(requested) => {
            if permitted.contains(requested) {
                Ok(requested.to_string())
            } else {
                Err(AdapterSelectionError::NotPermitted {
                    requested: requested.to_string(),
                    permitted: permitted.into_iter().collect(),
                })
            }
        }
        None => config
            .get("adapter")
            .or_else(|| config.get("adapter_type"))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            // A spec that declared only a single allowlisted adapter and no
            // explicit primary still has an unambiguous default.
            .or_else(|| {
                (permitted.len() == 1)
                    .then(|| permitted.iter().next().cloned())
                    .flatten()
            })
            .ok_or(AdapterSelectionError::Undeclared),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn entity_override_must_be_declared() {
        // ARN-228: integration declares only `http`; an entity asking for a
        // different native adapter is rejected, not silently honored.
        let cfg = config(&[("adapter", "http"), ("url", "https://example.test")]);
        let err = select_permitted_adapter(Some("codex"), &cfg)
            .expect_err("undeclared override must be rejected");
        assert!(matches!(err, AdapterSelectionError::NotPermitted { .. }));

        // The declared adapter itself is always allowed.
        assert_eq!(
            select_permitted_adapter(Some("http"), &cfg).ok(),
            Some("http".to_string())
        );
    }

    #[test]
    fn allowlist_permits_declared_switch() {
        // Dynamic selection still works when the spec opts in via allowed_adapters.
        let cfg = config(&[
            ("adapter", "http"),
            ("allowed_adapters", "http, claude_code"),
        ]);
        assert_eq!(
            select_permitted_adapter(Some("claude_code"), &cfg).ok(),
            Some("claude_code".to_string())
        );
        assert!(matches!(
            select_permitted_adapter(Some("codex"), &cfg),
            Err(AdapterSelectionError::NotPermitted { .. })
        ));
    }

    #[test]
    fn defaults_to_declared_primary_without_override() {
        let cfg = config(&[
            ("adapter", "http"),
            ("allowed_adapters", "http, claude_code"),
        ]);
        assert_eq!(
            select_permitted_adapter(None, &cfg).ok(),
            Some("http".to_string())
        );
        assert_eq!(
            select_permitted_adapter(Some("   "), &cfg).ok(),
            Some("http".to_string()),
            "blank override falls back to the declared primary"
        );
    }

    #[test]
    fn missing_declaration_is_undeclared() {
        let cfg = config(&[("url", "https://example.test")]);
        assert!(matches!(
            select_permitted_adapter(Some("codex"), &cfg),
            Err(AdapterSelectionError::Undeclared)
        ));
        assert!(matches!(
            select_permitted_adapter(None, &cfg),
            Err(AdapterSelectionError::Undeclared)
        ));
    }

    #[test]
    fn sole_allowlisted_adapter_is_the_default() {
        let cfg = config(&[("allowed_adapters", "openclaw")]);
        assert_eq!(
            select_permitted_adapter(None, &cfg).ok(),
            Some("openclaw".to_string())
        );
    }

    #[test]
    fn defaults_to_adapter_type_key_alone() {
        // A spec may declare the primary via `adapter_type` instead of `adapter`.
        let cfg = config(&[("adapter_type", "http")]);
        assert_eq!(
            select_permitted_adapter(None, &cfg).ok(),
            Some("http".to_string())
        );
        assert_eq!(
            select_permitted_adapter(Some("http"), &cfg).ok(),
            Some("http".to_string())
        );
        assert!(matches!(
            select_permitted_adapter(Some("codex"), &cfg),
            Err(AdapterSelectionError::NotPermitted { .. })
        ));
    }
}
