//! Core types for the reaction rule system.
//!
//! All types use `BTreeMap` for deterministic iteration order (DST compliance).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Maximum reaction rules per tenant (TigerStyle budget).
pub const MAX_REACTIONS_PER_TENANT: usize = 256;

/// Maximum cascade depth for recursive reaction dispatch (TigerStyle budget).
pub const MAX_REACTION_DEPTH: u32 = 8;

/// A reaction rule: when a trigger fires, dispatch an action on a target entity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReactionRule {
    /// Human-readable name for logging and debugging.
    pub name: String,
    /// The trigger condition (entity type + action + optional state).
    pub when: ReactionTrigger,
    /// The target action to dispatch.
    pub then: ReactionTarget,
    /// How to resolve the target entity ID.
    pub resolve_target: TargetResolver,
}

/// Trigger condition for a reaction rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReactionTrigger {
    /// The entity type that triggers this reaction (e.g., "Order").
    pub entity_type: String,
    /// The action name that triggers this reaction. `None` = any action.
    pub action: Option<String>,
    /// The target state after the action. `None` = any resulting state.
    pub to_state: Option<String>,
}

/// The target action to dispatch when a reaction fires.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReactionTarget {
    /// The entity type to dispatch the action on (e.g., "Payment").
    pub entity_type: String,
    /// The action to dispatch (e.g., "AuthorizePayment").
    pub action: String,
    /// Static parameters to pass to the target action.
    #[serde(default)]
    pub params: serde_json::Value,
    /// Dynamic params: target-param-name → source-entity-field-name.
    ///
    /// At dispatch time, each key is read from the source entity's fields and
    /// merged into the effective params object. Collides with `params` keys
    /// are rejected at registry parse time. A missing source field logs a
    /// warning and skips the key — the reaction still fires with a partial
    /// param map (consistent with `resolver::Field`'s `None`-on-missing posture).
    ///
    /// `BTreeMap` for deterministic iteration order (DST compliance).
    #[serde(default)]
    pub params_from: BTreeMap<String, String>,
}

/// How to resolve the target entity ID for a reaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum TargetResolver {
    /// Read the target entity ID from a field on the source entity.
    Field {
        /// The field name containing the target entity ID.
        field: String,
    },
    /// Use the same entity ID as the source.
    SameId,
    /// Use a static entity ID.
    Static {
        /// The fixed entity ID.
        entity_id: String,
    },
    /// Create the target entity if it doesn't exist, using a derived ID.
    CreateIfMissing {
        /// Field name containing the target entity ID (or derive from source).
        id_field: String,
    },
    /// Create a genuinely new target entity with a fresh UUID on every
    /// reaction dispatch.
    ///
    /// Unlike [`TargetResolver::CreateIfMissing`] — which keys off a field
    /// on the source entity and is intended for per-source-entity singletons
    /// (one FileVersion per File, etc.) — `Create` returns a new
    /// [`temper_runtime::scheduler::sim_uuid`] every time. This is the
    /// correct resolver for pipeline chaining: each source action spawns a
    /// brand new target entity instance.
    ///
    /// `sim_uuid()` (not `uuid::Uuid::new_v4`) is required for DST
    /// compliance — reaction cascades under `SimReactionSystem` must be
    /// deterministic across seeded runs.
    Create,
}

/// The result of dispatching a single reaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReactionResult {
    /// The rule that was triggered.
    pub rule_name: String,
    /// Whether the target action succeeded.
    pub success: bool,
    /// The target entity's status after the action (if available).
    pub target_status: Option<String>,
    /// Error message if the action failed.
    pub error: Option<String>,
    /// The cascade depth at which this reaction fired.
    pub depth: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reaction_rule_serializes_roundtrip() {
        let rule = ReactionRule {
            name: "order_confirmed_triggers_payment".to_string(),
            when: ReactionTrigger {
                entity_type: "Order".to_string(),
                action: Some("ConfirmOrder".to_string()),
                to_state: Some("Confirmed".to_string()),
            },
            then: ReactionTarget {
                entity_type: "Payment".to_string(),
                action: "AuthorizePayment".to_string(),
                params: serde_json::json!({}),
                params_from: BTreeMap::new(),
            },
            resolve_target: TargetResolver::Field {
                field: "payment_id".to_string(),
            },
        };

        let json = serde_json::to_string(&rule).unwrap();
        let deserialized: ReactionRule = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.name, rule.name);
        assert_eq!(deserialized.when.entity_type, "Order");
        assert_eq!(deserialized.then.action, "AuthorizePayment");
    }

    #[test]
    fn reaction_target_serialises_params_from() {
        let mut pf = BTreeMap::new();
        pf.insert("job_type".to_string(), "next_stage".to_string());
        let target = ReactionTarget {
            entity_type: "CurationJob".to_string(),
            action: "Submit".to_string(),
            params: serde_json::json!({}),
            params_from: pf,
        };
        let json = serde_json::to_string(&target).unwrap();
        assert!(json.contains("\"params_from\""));
        assert!(json.contains("\"job_type\":\"next_stage\""));

        let back: ReactionTarget = serde_json::from_str(&json).unwrap();
        assert_eq!(
            back.params_from.get("job_type").map(String::as_str),
            Some("next_stage")
        );
    }

    #[test]
    fn reaction_target_defaults_params_from_empty() {
        let json = r#"{"entity_type":"B","action":"Do"}"#;
        let target: ReactionTarget = serde_json::from_str(json).unwrap();
        assert!(target.params_from.is_empty());
    }

    #[test]
    fn target_resolver_variants_serialize() {
        let field = TargetResolver::Field {
            field: "payment_id".to_string(),
        };
        let json = serde_json::to_string(&field).unwrap();
        assert!(json.contains("\"type\":\"Field\""));

        let same = TargetResolver::SameId;
        let json = serde_json::to_string(&same).unwrap();
        assert!(json.contains("\"type\":\"SameId\""));

        let static_id = TargetResolver::Static {
            entity_id: "singleton".to_string(),
        };
        let json = serde_json::to_string(&static_id).unwrap();
        assert!(json.contains("\"type\":\"Static\""));

        let create = TargetResolver::CreateIfMissing {
            id_field: "payment_id".to_string(),
        };
        let json = serde_json::to_string(&create).unwrap();
        assert!(json.contains("\"type\":\"CreateIfMissing\""));

        let fresh = TargetResolver::Create;
        let json = serde_json::to_string(&fresh).unwrap();
        assert!(json.contains("\"type\":\"Create\""));
    }
}
