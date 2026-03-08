use serde::{Deserialize, Serialize};

/// Delete policy for relation integrity checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DeletePolicy {
    /// Reject parent delete when dependents exist.
    #[default]
    Restrict,
    /// Delete dependents automatically.
    Cascade,
    /// Set dependent FK field to null when possible.
    SetNull,
}

/// Cross-invariant strength.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum InvariantKind {
    /// Must hold at write-time.
    #[default]
    Hard,
    /// Should converge within a bounded window.
    Eventual,
}

/// A single cross-entity invariant rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrossInvariant {
    /// Stable invariant name.
    pub name: String,
    /// `hard` (default) or `eventual`.
    #[serde(default)]
    pub kind: InvariantKind,
    /// Trigger selector in the form `Entity.*` or `Entity.Action`.
    pub on: String,
    /// Invariant assertion string.
    #[serde(rename = "assert")]
    pub assertion: String,
    /// Required convergence window for eventual invariants.
    #[serde(default)]
    pub window_ms: Option<u64>,
}

/// Override relation behavior for a specific navigation property.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationOverride {
    pub from_entity: String,
    pub navigation_property: String,
    pub delete_policy: DeletePolicy,
}

/// Root TOML document for `cross-invariants.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrossInvariantSpec {
    /// Version marker for future compatibility.
    #[serde(default = "default_version")]
    pub version: u32,
    /// Default relation delete policy.
    #[serde(default)]
    pub default_delete_policy: DeletePolicy,
    /// Invariant rules.
    #[serde(default, rename = "invariant")]
    pub invariants: Vec<CrossInvariant>,
    /// Relation policy overrides.
    #[serde(default, rename = "relation_override")]
    pub relation_overrides: Vec<RelationOverride>,
}

impl Default for CrossInvariantSpec {
    fn default() -> Self {
        Self {
            version: default_version(),
            default_delete_policy: DeletePolicy::Restrict,
            invariants: Vec::new(),
            relation_overrides: Vec::new(),
        }
    }
}

fn default_version() -> u32 {
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delete_policy_default_is_restrict() {
        assert_eq!(DeletePolicy::default(), DeletePolicy::Restrict);
    }

    #[test]
    fn invariant_kind_default_is_hard() {
        assert_eq!(InvariantKind::default(), InvariantKind::Hard);
    }

    #[test]
    fn cross_invariant_spec_default() {
        let spec = CrossInvariantSpec::default();
        assert_eq!(spec.version, 1);
        assert_eq!(spec.default_delete_policy, DeletePolicy::Restrict);
        assert!(spec.invariants.is_empty());
        assert!(spec.relation_overrides.is_empty());
    }

    #[test]
    fn delete_policy_serde_roundtrip() {
        for policy in [
            DeletePolicy::Restrict,
            DeletePolicy::Cascade,
            DeletePolicy::SetNull,
        ] {
            let json = serde_json::to_string(&policy).unwrap();
            let back: DeletePolicy = serde_json::from_str(&json).unwrap();
            assert_eq!(back, policy);
        }
    }

    #[test]
    fn delete_policy_serde_rename() {
        assert_eq!(
            serde_json::to_string(&DeletePolicy::Restrict).unwrap(),
            "\"restrict\""
        );
        assert_eq!(
            serde_json::to_string(&DeletePolicy::Cascade).unwrap(),
            "\"cascade\""
        );
        assert_eq!(
            serde_json::to_string(&DeletePolicy::SetNull).unwrap(),
            "\"setnull\""
        );
    }

    #[test]
    fn invariant_kind_serde_rename() {
        assert_eq!(
            serde_json::to_string(&InvariantKind::Hard).unwrap(),
            "\"hard\""
        );
        assert_eq!(
            serde_json::to_string(&InvariantKind::Eventual).unwrap(),
            "\"eventual\""
        );
    }

    #[test]
    fn cross_invariant_toml_roundtrip() {
        let toml_src = r#"
version = 1
default_delete_policy = "cascade"

[[invariant]]
name = "OrderRequiresCustomer"
kind = "hard"
on = "Order.*"
assert = "Customer.status in [Active]"

[[relation_override]]
from_entity = "Order"
navigation_property = "Items"
delete_policy = "cascade"
"#;
        let spec: CrossInvariantSpec = toml::from_str(toml_src).unwrap();
        assert_eq!(spec.version, 1);
        assert_eq!(spec.default_delete_policy, DeletePolicy::Cascade);
        assert_eq!(spec.invariants.len(), 1);
        assert_eq!(spec.invariants[0].name, "OrderRequiresCustomer");
        assert_eq!(spec.invariants[0].kind, InvariantKind::Hard);
        assert_eq!(spec.invariants[0].on, "Order.*");
        assert_eq!(spec.relation_overrides.len(), 1);
        assert_eq!(
            spec.relation_overrides[0].delete_policy,
            DeletePolicy::Cascade
        );
    }

    #[test]
    fn eventual_invariant_with_window() {
        let toml_src = r#"
[[invariant]]
name = "EventualSync"
kind = "eventual"
on = "Entity.Action"
assert = "synced == true"
window_ms = 5000
"#;
        let spec: CrossInvariantSpec = toml::from_str(toml_src).unwrap();
        assert_eq!(spec.invariants[0].kind, InvariantKind::Eventual);
        assert_eq!(spec.invariants[0].window_ms, Some(5000));
    }

    #[test]
    fn minimal_spec_defaults() {
        let toml_src = r#"
[[invariant]]
name = "Min"
on = "E.*"
assert = "x > 0"
"#;
        let spec: CrossInvariantSpec = toml::from_str(toml_src).unwrap();
        assert_eq!(spec.version, 1);
        assert_eq!(spec.default_delete_policy, DeletePolicy::Restrict);
        assert_eq!(spec.invariants[0].kind, InvariantKind::Hard);
        assert_eq!(spec.invariants[0].window_ms, None);
    }
}
