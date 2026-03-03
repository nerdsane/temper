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
