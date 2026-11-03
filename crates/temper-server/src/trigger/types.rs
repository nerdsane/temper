//! Core types for the reaction rule system.
//!
//! All types use `BTreeMap` for deterministic iteration order (DST compliance).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Advisory threshold for a tenant's reaction-rule count. **Not a maximum.**
///
/// [`register_tenant_rules`](super::registry::ReactionRegistry::register_tenant_rules)
/// warns above this number and registers every rule anyway. It asserted until
/// 2026-09-10, when a tenant's fifteenth app took it to 265 rules and the panic
/// crash-looped the platform at startup. Nothing is preallocated from this
/// constant: rules live in growable `BTreeMap`s, so exceeding it corrupts
/// nothing. A consumer must not reject a tenant for being above it. See
/// ADR-0176.
pub const MAX_REACTIONS_PER_TENANT: usize = 256;

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
    /// Optional principal (registered AgentType name) for elevation
    /// (ADR-0046). When `Some`, dispatch uses a synthetic
    /// `SecurityContext` with every Cedar attribute (`role`, `agent_type`,
    /// `id`) populated from this name. When `None`, dispatch inherits
    /// the invoking principal — the trigger runs as whoever called the
    /// source action.
    #[serde(default)]
    pub principal: Option<String>,
}

/// Trigger condition for a reaction rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReactionTrigger {
    /// The entity type that triggers this reaction (e.g., "Order").
    pub entity_type: String,
    /// The action name that triggers this reaction. `None` = any action.
    pub action: Option<String>,
    /// Optional guard over the source entity's post-action fields and status
    /// (and related entities' statuses). When `Some`, the reaction only fires
    /// if the guard holds. Guard-skipped rules do NOT emit a
    /// [`ReactionResult`] — they never fired.
    #[serde(default)]
    pub guard: Option<temper_spec::predicate::Expr>,
}

/// The target action to dispatch when a reaction fires.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReactionTarget {
    /// The entity type to dispatch the action on (e.g., "Payment").
    pub entity_type: String,
    /// The action to dispatch (e.g., "AuthorizePayment").
    pub action: String,
    /// Parameters for the target action: literals, or fields of the source
    /// entity read after it commits (`Id` is its id). A field the source does
    /// not have leaves its key out — the reaction still fires with the other
    /// parameters (consistent with `resolver::Field`'s `None`-on-missing
    /// posture).
    ///
    /// `BTreeMap` for deterministic iteration order (DST compliance).
    #[serde(default)]
    pub args: BTreeMap<String, temper_spec::predicate::Arg>,
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
    /// How many reactions deep in its chain this one fired; the first
    /// reaction of an action is at 0.
    pub depth: u32,
}

impl ReactionResult {
    /// A reaction that was not run because it would repeat a step its chain
    /// is already in ([`InlineChain`](crate::request_context::InlineChain)).
    pub fn stopped_on_repeat(
        rule_name: &str,
        depth: u32,
        repeat: &crate::request_context::RepeatedStep,
    ) -> Self {
        Self {
            rule_name: rule_name.to_string(),
            success: false,
            target_status: None,
            error: Some(format!(
                "reaction not run: it would repeat a step its chain is already in ({repeat}), \
                 so the chain is a loop"
            )),
            depth,
        }
    }
}
