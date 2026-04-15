//! Context models for trajectories
//!
//! DST adaptation: `OTSEntity.metadata` uses `BTreeMap` for deterministic
//! iteration order.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Entity referenced in trajectory context
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OTSEntity {
    /// Entity type (e.g., 'tool', 'resource', custom types)
    #[serde(rename = "type")]
    pub entity_type: String,

    /// Entity identifier
    pub id: String,

    /// Human-readable name
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Type-specific attributes (BTreeMap for deterministic iteration)
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, serde_json::Value>,
}

impl OTSEntity {
    /// Create a new entity with the given type and id
    pub fn new(entity_type: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            entity_type: entity_type.into(),
            id: id.into(),
            name: None,
            metadata: BTreeMap::new(),
        }
    }

    /// Set the name
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Add metadata key-value pair
    pub fn with_metadata(mut self, key: impl Into<String>, value: serde_json::Value) -> Self {
        self.metadata.insert(key.into(), value);
        self
    }
}

/// Resource accessed during trajectory
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OTSResource {
    /// Resource type (e.g., 'file', 'api', 'database')
    #[serde(rename = "type")]
    pub resource_type: String,

    /// Resource URI
    pub uri: String,

    /// When resource was accessed
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accessed_at: Option<DateTime<Utc>>,
}

impl OTSResource {
    /// Create a new resource with the given type and URI
    pub fn new(resource_type: impl Into<String>, uri: impl Into<String>) -> Self {
        Self {
            resource_type: resource_type.into(),
            uri: uri.into(),
            accessed_at: None,
        }
    }

    /// Set the access timestamp
    pub fn with_accessed_at(mut self, accessed_at: DateTime<Utc>) -> Self {
        self.accessed_at = Some(accessed_at);
        self
    }
}

/// User context
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OTSUser {
    /// User identifier
    pub id: String,

    /// User handle or username
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handle: Option<String>,

    /// Organization identifier
    #[serde(skip_serializing_if = "Option::is_none")]
    pub org_id: Option<String>,

    /// Team memberships
    #[serde(skip_serializing_if = "Option::is_none")]
    pub teams: Option<Vec<String>>,

    /// User timezone
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
}

impl OTSUser {
    /// Create a new user with the given id
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            handle: None,
            org_id: None,
            teams: None,
            timezone: None,
        }
    }

    /// Set the handle
    pub fn with_handle(mut self, handle: impl Into<String>) -> Self {
        self.handle = Some(handle.into());
        self
    }

    /// Set the organization id
    pub fn with_org_id(mut self, org_id: impl Into<String>) -> Self {
        self.org_id = Some(org_id.into());
        self
    }

    /// Set the teams
    pub fn with_teams(mut self, teams: Vec<String>) -> Self {
        self.teams = Some(teams);
        self
    }

    /// Set the timezone
    pub fn with_timezone(mut self, timezone: impl Into<String>) -> Self {
        self.timezone = Some(timezone.into());
        self
    }
}

/// Initial context for trajectory
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OTSContext {
    /// URL or path where agent was invoked
    #[serde(skip_serializing_if = "Option::is_none")]
    pub referrer: Option<String>,

    /// User context
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<OTSUser>,

    /// Entities in context
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entities: Vec<OTSEntity>,

    /// Resources accessed
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resources: Vec<OTSResource>,

    /// Framework-specific context
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_context: Option<String>,
}

impl Default for OTSContext {
    fn default() -> Self {
        Self::new()
    }
}

impl OTSContext {
    /// Create a new empty context
    pub fn new() -> Self {
        Self {
            referrer: None,
            user: None,
            entities: Vec::new(),
            resources: Vec::new(),
            custom_context: None,
        }
    }

    /// Set the referrer
    pub fn with_referrer(mut self, referrer: impl Into<String>) -> Self {
        self.referrer = Some(referrer.into());
        self
    }

    /// Set the user
    pub fn with_user(mut self, user: OTSUser) -> Self {
        self.user = Some(user);
        self
    }

    /// Add an entity
    pub fn with_entity(mut self, entity: OTSEntity) -> Self {
        self.entities.push(entity);
        self
    }

    /// Add a resource
    pub fn with_resource(mut self, resource: OTSResource) -> Self {
        self.resources.push(resource);
        self
    }

    /// Set custom context
    pub fn with_custom_context(mut self, custom_context: impl Into<String>) -> Self {
        self.custom_context = Some(custom_context.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_entity_serialization() {
        let entity = OTSEntity::new("tool", "calculator")
            .with_name("Calculator Tool")
            .with_metadata("version", json!("1.0"));

        let json_str = serde_json::to_string(&entity).unwrap();
        let parsed: OTSEntity = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed, entity);

        // Verify snake_case in JSON
        assert!(json_str.contains(r#""type":"tool""#));
    }

    #[test]
    fn test_resource_serialization() {
        let resource = OTSResource::new("api", "https://api.example.com/data");

        let json_str = serde_json::to_string(&resource).unwrap();
        let parsed: OTSResource = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed, resource);
    }

    #[test]
    fn test_user_serialization() {
        let user = OTSUser::new("user_123")
            .with_handle("alice")
            .with_org_id("org_456")
            .with_teams(vec!["engineering".to_string(), "ml".to_string()])
            .with_timezone("America/Los_Angeles");

        let json_str = serde_json::to_string(&user).unwrap();
        let parsed: OTSUser = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed, user);
    }

    #[test]
    fn test_context_serialization() {
        let entity = OTSEntity::new("tool", "search");
        let resource = OTSResource::new("database", "postgresql://localhost/db");
        let user = OTSUser::new("user_789");

        let context = OTSContext::new()
            .with_referrer("https://app.example.com")
            .with_user(user)
            .with_entity(entity)
            .with_resource(resource);

        let json_str = serde_json::to_string(&context).unwrap();
        let parsed: OTSContext = serde_json::from_str(&json_str).unwrap();
        assert_eq!(parsed, context);
    }

    #[test]
    fn test_empty_context_omits_fields() {
        let context = OTSContext::new();
        let json_str = serde_json::to_string(&context).unwrap();

        // Empty vecs and None should not appear
        assert_eq!(json_str, "{}");
    }

    #[test]
    fn test_entity_without_optional_fields() {
        let entity = OTSEntity::new("resource", "file_1");
        let json_str = serde_json::to_string(&entity).unwrap();

        // Should not include name or metadata
        assert!(!json_str.contains("name"));
        assert!(!json_str.contains("metadata"));
    }
}
