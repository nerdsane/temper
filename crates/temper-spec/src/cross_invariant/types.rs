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
    /// Set dependent FK field to null when possible. Not implemented: every
    /// parser refuses it, so no relation carries it.
    SetNull,
    /// Leave dependents as they are: the relation is navigation only, and a
    /// dependent may keep pointing at a deleted parent.
    None,
}

impl DeletePolicy {
    /// Whether the server carries out this policy when a parent is deleted.
    pub fn is_implemented(self) -> bool {
        !matches!(self, Self::SetNull)
    }

    /// The name specs write: the serde name, which [`str::parse`] reads back.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Restrict => "restrict",
            Self::Cascade => "cascade",
            Self::SetNull => "setnull",
            Self::None => "none",
        }
    }
}

impl std::str::FromStr for DeletePolicy {
    type Err = String;

    /// Parse the names `cross-invariants.toml` uses: `restrict`, `cascade`,
    /// `none` and `setnull`.
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "restrict" => Ok(Self::Restrict),
            "cascade" => Ok(Self::Cascade),
            "none" => Ok(Self::None),
            "setnull" => Ok(Self::SetNull),
            other => Err(format!(
                "unknown delete policy '{other}' (expected restrict, cascade or none)"
            )),
        }
    }
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
        assert_eq!(
            serde_json::to_string(&DeletePolicy::None).unwrap(),
            "\"none\""
        );
    }

    #[test]
    fn delete_policy_parses_the_names_it_serializes_to() {
        for policy in [
            DeletePolicy::Restrict,
            DeletePolicy::Cascade,
            DeletePolicy::SetNull,
            DeletePolicy::None,
        ] {
            let name = serde_json::to_value(policy).unwrap();
            assert_eq!(name.as_str(), Some(policy.as_str()));
            let parsed: DeletePolicy = policy.as_str().parse().unwrap();
            assert_eq!(parsed, policy);
        }
        assert!("Cascade".parse::<DeletePolicy>().is_err());
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
