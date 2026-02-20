//! Multi-tenancy types for the Temper platform.
//!
//! A [`TenantId`] scopes all resources (actors, events, specs, cache keys)
//! to a tenant. Single-tenant deployments use [`TenantId::default()`] which
//! returns `"default"`.
//!
//! A [`QualifiedEntityId`] is the globally unique identity of an entity:
//! `tenant:entity_type:entity_id`. It replaces the old `entity_type:entity_id`
//! convention while remaining backward-compatible via the `"default"` tenant.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A tenant identifier scoping all platform resources.
///
/// Convention: lowercase, alphanumeric + hyphens (e.g., `"alpha"`, `"beta"`, `"my-app"`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TenantId(String);

impl TenantId {
    /// Create a new tenant ID.
    ///
    /// # Panics
    ///
    /// Panics if the tenant ID is empty or contains colons (which are used as
    /// separators in persistence IDs).
    pub fn new(id: impl Into<String>) -> Self {
        let id = id.into();
        assert!(!id.is_empty(), "tenant ID must not be empty");
        assert!(!id.contains(':'), "tenant ID must not contain colons: {id}");
        Self(id)
    }

    /// The default tenant for single-tenant deployments.
    pub fn default_tenant() -> Self {
        Self("default".to_string())
    }

    /// The raw string value.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for TenantId {
    fn default() -> Self {
        Self::default_tenant()
    }
}

impl fmt::Display for TenantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for TenantId {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}

impl From<String> for TenantId {
    fn from(s: String) -> Self {
        Self::new(s)
    }
}

/// The globally unique identity of an entity in the platform.
///
/// Format: `tenant:entity_type:entity_id` (e.g., `"my-app:Order:abc-123"`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct QualifiedEntityId {
    /// The tenant this entity belongs to.
    pub tenant: TenantId,
    /// The entity type (e.g., "Order", "Task").
    pub entity_type: String,
    /// The entity ID within the type (e.g., "abc-123").
    pub entity_id: String,
}

impl QualifiedEntityId {
    /// Create a new qualified entity ID.
    pub fn new(
        tenant: impl Into<TenantId>,
        entity_type: impl Into<String>,
        entity_id: impl Into<String>,
    ) -> Self {
        Self {
            tenant: tenant.into(),
            entity_type: entity_type.into(),
            entity_id: entity_id.into(),
        }
    }

    /// Create a qualified entity ID in the default tenant.
    pub fn in_default_tenant(entity_type: impl Into<String>, entity_id: impl Into<String>) -> Self {
        Self::new(TenantId::default(), entity_type, entity_id)
    }

    /// The persistence ID string: `tenant:entity_type:entity_id`.
    pub fn persistence_id(&self) -> String {
        format!("{}:{}:{}", self.tenant, self.entity_type, self.entity_id)
    }

    /// Parse a persistence ID string back into a [`QualifiedEntityId`].
    ///
    /// Accepts both the new 3-segment format (`tenant:type:id`) and the
    /// legacy 2-segment format (`type:id`) for backward compatibility.
    /// Legacy IDs are assigned to the `"default"` tenant.
    pub fn parse(persistence_id: &str) -> Result<Self, String> {
        let segments: Vec<&str> = persistence_id.splitn(3, ':').collect();
        match segments.len() {
            3 => {
                let tenant = segments[0];
                let entity_type = segments[1];
                let entity_id = segments[2];
                if tenant.is_empty() || entity_type.is_empty() || entity_id.is_empty() {
                    return Err(format!(
                        "invalid persistence_id (empty segment): {persistence_id}"
                    ));
                }
                Ok(Self {
                    tenant: TenantId::new(tenant),
                    entity_type: entity_type.to_string(),
                    entity_id: entity_id.to_string(),
                })
            }
            2 => {
                // Legacy format: entity_type:entity_id → default tenant
                let entity_type = segments[0];
                let entity_id = segments[1];
                if entity_type.is_empty() || entity_id.is_empty() {
                    return Err(format!(
                        "invalid persistence_id (empty segment): {persistence_id}"
                    ));
                }
                Ok(Self {
                    tenant: TenantId::default(),
                    entity_type: entity_type.to_string(),
                    entity_id: entity_id.to_string(),
                })
            }
            _ => Err(format!(
                "invalid persistence_id (expected 'tenant:type:id' or 'type:id'): {persistence_id}"
            )),
        }
    }

    /// Actor registry key: `tenant:entity_type:entity_id`.
    pub fn actor_key(&self) -> String {
        self.persistence_id()
    }
}

impl fmt::Display for QualifiedEntityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", self.tenant, self.entity_type, self.entity_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tenant_id_default() {
        let t = TenantId::default();
        assert_eq!(t.as_str(), "default");
    }

    #[test]
    fn tenant_id_custom() {
        let t = TenantId::new("alpha");
        assert_eq!(t.as_str(), "alpha");
        assert_eq!(t.to_string(), "alpha");
    }

    #[test]
    #[should_panic(expected = "must not be empty")]
    fn tenant_id_empty_panics() {
        TenantId::new("");
    }

    #[test]
    #[should_panic(expected = "must not contain colons")]
    fn tenant_id_colon_panics() {
        TenantId::new("a:b");
    }

    #[test]
    fn tenant_id_from_str() {
        let t: TenantId = "beta".into();
        assert_eq!(t.as_str(), "beta");
    }

    #[test]
    fn qualified_entity_id_persistence_id() {
        let qid = QualifiedEntityId::new("alpha", "Order", "abc-123");
        assert_eq!(qid.persistence_id(), "alpha:Order:abc-123");
    }

    #[test]
    fn qualified_entity_id_default_tenant() {
        let qid = QualifiedEntityId::in_default_tenant("Order", "abc-123");
        assert_eq!(qid.persistence_id(), "default:Order:abc-123");
        assert_eq!(qid.tenant, TenantId::default());
    }

    #[test]
    fn parse_3_segment_persistence_id() {
        let qid = QualifiedEntityId::parse("alpha:Order:abc-123").unwrap();
        assert_eq!(qid.tenant.as_str(), "alpha");
        assert_eq!(qid.entity_type, "Order");
        assert_eq!(qid.entity_id, "abc-123");
    }

    #[test]
    fn parse_legacy_2_segment_persistence_id() {
        let qid = QualifiedEntityId::parse("Order:abc-123").unwrap();
        assert_eq!(qid.tenant.as_str(), "default");
        assert_eq!(qid.entity_type, "Order");
        assert_eq!(qid.entity_id, "abc-123");
    }

    #[test]
    fn parse_3_segment_with_colons_in_id() {
        // entity_id can contain colons (UUIDs, compound keys)
        let qid = QualifiedEntityId::parse("beta:Task:T-1:sub").unwrap();
        assert_eq!(qid.tenant.as_str(), "beta");
        assert_eq!(qid.entity_type, "Task");
        assert_eq!(qid.entity_id, "T-1:sub");
    }

    #[test]
    fn parse_empty_segment_fails() {
        assert!(QualifiedEntityId::parse(":Order:abc").is_err());
        assert!(QualifiedEntityId::parse("tenant::abc").is_err());
        assert!(QualifiedEntityId::parse("tenant:Order:").is_err());
    }

    #[test]
    fn parse_no_colon_fails() {
        assert!(QualifiedEntityId::parse("OrderAbc123").is_err());
    }

    #[test]
    fn actor_key_matches_persistence_id() {
        let qid = QualifiedEntityId::new("alpha", "Order", "abc-123");
        assert_eq!(qid.actor_key(), qid.persistence_id());
    }

    #[test]
    fn display_format() {
        let qid = QualifiedEntityId::new("beta", "Task", "T-42");
        assert_eq!(format!("{qid}"), "beta:Task:T-42");
    }

    #[test]
    fn ordering_is_tenant_first() {
        let a = QualifiedEntityId::new("alpha", "Order", "1");
        let b = QualifiedEntityId::new("beta", "Order", "1");
        assert!(a < b);
    }
}
