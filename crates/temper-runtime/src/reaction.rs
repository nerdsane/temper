//! Reaction rule types and registry for actor coordination.
//!
//! Reaction rules are external configuration that wire actors together:
//! "when actor X emits Y, tell actor Z action A".
//! This keeps IOA specs pure (single-entity) while enabling multi-actor workflows.
//!
//! # Example
//!
//! ```toml
//! [[reaction]]
//! name = "agent_requests_context"
//! when = { entity_type = "Agent", action = "StartProcess" }
//! then = { entity_type = "ContextManager", action = "PrepareContext" }
//! resolve_target = { type = "SameId" }
//! ```

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Maximum reaction rules per actor type (TigerStyle budget).
pub const MAX_REACTIONS_PER_ACTOR: usize = 256;

/// Maximum cascade depth for recursive reaction dispatch.
pub const MAX_REACTION_DEPTH: u32 = 8;

/// A reaction rule: when actor X emits Y, tell actor Z action A.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReactionRule {
    /// Human-readable name for logging and debugging.
    pub name: String,
    /// The trigger condition (actor type + emit name).
    pub when: ReactionTrigger,
    /// The target action to dispatch.
    pub then: ReactionTarget,
    /// How to resolve the target actor (defaults to SameId for same-namespace).
    pub resolve_target: TargetResolver,
}

/// Trigger condition for a reaction rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReactionTrigger {
    /// The actor type that emits (e.g., "Agent").
    pub entity_type: String,
    /// The emit/action name that triggers this reaction. None = any action.
    pub action: Option<String>,
    /// Optional: only trigger when actor is in this state after the action.
    pub to_state: Option<String>,
}

/// The target action to dispatch when a reaction fires.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReactionTarget {
    /// The actor type to tell (e.g., "ContextManager").
    pub entity_type: String,
    /// The action/message to send (e.g., "PrepareContext").
    pub action: String,
}

/// How to resolve the target actor's namespace/ID.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(tag = "type")]
pub enum TargetResolver {
    /// Same namespace as the source actor (most common for session actors).
    #[default]
    SameId,
    /// Read target namespace from a field in the source actor's state.
    Field { field: String },
    /// Use a static fixed namespace.
    Static { entity_id: String },
    /// Create the target if it doesn't exist.
    CreateIfMissing { id_field: String },
}

/// Registry of reaction rules, indexed by "ActorType:EmitName" for fast lookup.
///
/// No tenant isolation — namespace already encodes tenant context.
#[derive(Debug, Clone, Default)]
pub struct ReactionRegistry {
    /// Index: key = "ActorType:EmitName" or "ActorType:*"
    index: BTreeMap<String, Vec<ReactionRule>>,
}

impl ReactionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register reaction rules.
    pub fn register(&mut self, rules: Vec<ReactionRule>) {
        assert!(
            rules.len() <= MAX_REACTIONS_PER_ACTOR,
            "Reaction rules exceed budget of {MAX_REACTIONS_PER_ACTOR}"
        );
        for rule in rules {
            let key = match &rule.when.action {
                Some(action) => format!("{}:{}", rule.when.entity_type, action),
                None => format!("{}:*", rule.when.entity_type),
            };
            self.index.entry(key).or_default().push(rule);
        }
    }

    /// Look up matching reaction rules for (actor_type, emit_name, to_state).
    pub fn lookup(&self, actor_type: &str, emit_name: &str, to_state: &str) -> Vec<&ReactionRule> {
        let exact = format!("{actor_type}:{emit_name}");
        let wildcard = format!("{actor_type}:*");
        let mut results = Vec::new();

        for key in [&exact, &wildcard] {
            if let Some(rules) = self.index.get(key) {
                for rule in rules {
                    if matches_state_filter(rule, to_state) {
                        results.push(rule);
                    }
                }
            }
        }

        results
    }

    /// Is the registry empty?
    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }
}

fn matches_state_filter(rule: &ReactionRule, to_state: &str) -> bool {
    match &rule.when.to_state {
        Some(expected) => expected == to_state,
        None => true,
    }
}

// ─── TOML parsing ─────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ReactionsFile {
    #[serde(default)]
    reaction: Vec<ReactionToml>,
}

#[derive(Deserialize)]
struct ReactionToml {
    name: String,
    when: TriggerToml,
    then: TargetToml,
    #[serde(default)]
    resolve_target: ResolverToml,
}

#[derive(Deserialize)]
struct TriggerToml {
    entity_type: String,
    action: Option<String>,
    to_state: Option<String>,
}

#[derive(Deserialize)]
struct TargetToml {
    entity_type: String,
    action: String,
}

#[derive(Deserialize, Default)]
struct ResolverToml {
    #[serde(rename = "type", default = "default_resolver_type")]
    resolver_type: String,
    field: Option<String>,
    entity_id: Option<String>,
    id_field: Option<String>,
}

fn default_resolver_type() -> String {
    "SameId".to_string()
}

/// Parse reaction rules from a TOML string.
pub fn parse_reactions(toml_str: &str) -> Result<Vec<ReactionRule>, String> {
    let file: ReactionsFile =
        toml::from_str(toml_str).map_err(|e| format!("failed to parse reactions: {e}"))?;

    let mut rules = Vec::new();
    for r in file.reaction {
        let resolve_target = match r.resolve_target.resolver_type.as_str() {
            "SameId" | "same_id" => TargetResolver::SameId,
            "Field" | "field" => {
                let field = r.resolve_target.field.ok_or_else(|| {
                    format!("reaction '{}': 'Field' resolver requires 'field'", r.name)
                })?;
                TargetResolver::Field { field }
            }
            "Static" | "static" => {
                let entity_id = r.resolve_target.entity_id.ok_or_else(|| {
                    format!(
                        "reaction '{}': 'Static' resolver requires 'entity_id'",
                        r.name
                    )
                })?;
                TargetResolver::Static { entity_id }
            }
            "CreateIfMissing" | "create_if_missing" => {
                let id_field = r.resolve_target.id_field.ok_or_else(|| {
                    format!(
                        "reaction '{}': 'CreateIfMissing' requires 'id_field'",
                        r.name
                    )
                })?;
                TargetResolver::CreateIfMissing { id_field }
            }
            other => return Err(format!("unknown resolver type '{other}'")),
        };

        rules.push(ReactionRule {
            name: r.name,
            when: ReactionTrigger {
                entity_type: r.when.entity_type,
                action: r.when.action,
                to_state: r.when.to_state,
            },
            then: ReactionTarget {
                entity_type: r.then.entity_type,
                action: r.then.action,
            },
            resolve_target,
        });
    }

    Ok(rules)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(from_type: &str, emit: &str, to_type: &str, to_action: &str) -> ReactionRule {
        ReactionRule {
            name: format!("{from_type}_{emit}_to_{to_type}"),
            when: ReactionTrigger {
                entity_type: from_type.to_string(),
                action: Some(emit.to_string()),
                to_state: None,
            },
            then: ReactionTarget {
                entity_type: to_type.to_string(),
                action: to_action.to_string(),
            },
            resolve_target: TargetResolver::SameId,
        }
    }

    #[test]
    fn test_lookup_exact() {
        let mut reg = ReactionRegistry::new();
        reg.register(vec![rule(
            "Agent",
            "PrepareContext",
            "ContextManager",
            "PrepareContext",
        )]);
        let results = reg.lookup("Agent", "PrepareContext", "");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].then.entity_type, "ContextManager");
    }

    #[test]
    fn test_no_match() {
        let reg = ReactionRegistry::new();
        assert!(reg.lookup("Agent", "PrepareContext", "").is_empty());
    }

    #[test]
    fn test_parse_reactions_toml() {
        let toml = r#"
[[reaction]]
name = "agent_requests_context"
[reaction.when]
entity_type = "Agent"
action = "StartProcess"
[reaction.then]
entity_type = "ContextManager"
action = "PrepareContext"
[reaction.resolve_target]
type = "SameId"
"#;
        let rules = parse_reactions(toml).unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].name, "agent_requests_context");
        assert_eq!(rules[0].then.entity_type, "ContextManager");
        assert_eq!(rules[0].resolve_target, TargetResolver::SameId);
    }

    #[test]
    fn test_parse_empty() {
        assert!(parse_reactions("").unwrap().is_empty());
    }
}
