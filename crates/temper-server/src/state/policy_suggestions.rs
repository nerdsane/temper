//! Denial pattern detection for Cedar policy suggestions.
//!
//! Pure deterministic counting — no LLM. Tracks denial patterns by
//! `(agent_type, action, resource_type)` and suggests Cedar policies when
//! a pattern crosses a configurable threshold.

use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

/// A single denial pattern for a specific (agent_type, action, resource_type) triple.
#[derive(Debug, Clone, Serialize)]
pub struct DenialPattern {
    /// Agent type (None for untyped agents).
    pub agent_type: Option<String>,
    /// Action that was denied.
    pub action: String,
    /// Resource type that was denied.
    pub resource_type: String,
    /// Total denial count.
    pub count: usize,
    /// First denial timestamp.
    pub first_seen: String,
    /// Most recent denial timestamp.
    pub last_seen: String,
    /// Distinct resource IDs denied under this pattern.
    pub distinct_resource_ids: BTreeSet<String>,
}

/// A grouped pattern across multiple denied actions on the same resource type.
#[derive(Debug, Clone, Serialize)]
pub struct GroupedPattern {
    /// Agent type (None for untyped agents).
    pub agent_type: Option<String>,
    /// Resource type with multiple denied actions.
    pub resource_type: String,
    /// Distinct actions that were denied.
    pub denied_actions: BTreeSet<String>,
    /// Total denials across all actions.
    pub total_denials: usize,
}

/// A suggested Cedar policy derived from denial patterns.
#[derive(Debug, Clone, Serialize)]
pub struct PolicySuggestion {
    /// Human-readable description.
    pub description: String,
    /// The suggested scope matrix.
    pub matrix: temper_authz::PolicyScopeMatrix,
    /// Preview Cedar policy text.
    pub cedar_preview: String,
    /// Number of denials that triggered this suggestion.
    pub denial_count: usize,
    /// Whether this is a grouped (cross-action) suggestion.
    pub grouped: bool,
}

/// Engine that detects denial patterns and generates policy suggestions.
///
/// Two levels of pattern detection:
/// - **Per-action**: counts denials by `(agent_type, action, resource_type)`.
///   When count >= `action_threshold`, suggests a per-action policy.
/// - **Cross-action**: when `group_threshold`+ distinct actions on the same
///   `(agent_type, resource_type)` are each denied, suggests a broader policy.
pub struct PolicySuggestionEngine {
    /// Per-action suggestion threshold (default: 3).
    action_threshold: usize,
    /// Cross-action grouping threshold (default: 3 distinct actions).
    group_threshold: usize,
    /// Per-action denial patterns.
    per_action: BTreeMap<(Option<String>, String, String), DenialPattern>,
    /// Cross-action grouping.
    per_type: BTreeMap<(Option<String>, String), GroupedPattern>,
}

impl PolicySuggestionEngine {
    /// Create a new engine with default thresholds.
    pub fn new() -> Self {
        Self {
            action_threshold: 3,
            group_threshold: 3,
            per_action: BTreeMap::new(),
            per_type: BTreeMap::new(),
        }
    }

    /// Record a denial event.
    pub fn record_denial(
        &mut self,
        agent_type: Option<&str>,
        action: &str,
        resource_type: &str,
        resource_id: &str,
        timestamp: &str,
    ) {
        let key = (
            agent_type.map(String::from),
            action.to_string(),
            resource_type.to_string(),
        );

        let pattern = self
            .per_action
            .entry(key.clone())
            .or_insert_with(|| DenialPattern {
                agent_type: agent_type.map(String::from),
                action: action.to_string(),
                resource_type: resource_type.to_string(),
                count: 0,
                first_seen: timestamp.to_string(),
                last_seen: timestamp.to_string(),
                distinct_resource_ids: BTreeSet::new(),
            });
        pattern.count += 1;
        pattern.last_seen = timestamp.to_string();
        pattern
            .distinct_resource_ids
            .insert(resource_id.to_string());

        // Update cross-action grouping.
        let type_key = (agent_type.map(String::from), resource_type.to_string());
        let grouped = self
            .per_type
            .entry(type_key)
            .or_insert_with(|| GroupedPattern {
                agent_type: agent_type.map(String::from),
                resource_type: resource_type.to_string(),
                denied_actions: BTreeSet::new(),
                total_denials: 0,
            });
        grouped.denied_actions.insert(action.to_string());
        grouped.total_denials += 1;
    }

    /// Generate policy suggestions from accumulated denial patterns.
    ///
    /// Returns grouped suggestions where applicable, individual suggestions otherwise.
    /// A grouped suggestion replaces its individual constituents.
    pub fn suggestions(&self) -> Vec<PolicySuggestion> {
        let mut suggestions = Vec::new();
        let mut grouped_keys: BTreeSet<(Option<String>, String)> = BTreeSet::new();

        // Check for cross-action groupings first.
        for (key, grouped) in &self.per_type {
            if grouped.denied_actions.len() >= self.group_threshold {
                grouped_keys.insert(key.clone());
                let matrix = temper_authz::PolicyScopeMatrix {
                    principal: if grouped.agent_type.is_some() {
                        temper_authz::PrincipalScope::AgentsOfType
                    } else {
                        temper_authz::PrincipalScope::AnyAgent
                    },
                    action: temper_authz::ActionScope::AllActionsOnType,
                    resource: temper_authz::ResourceScope::AnyOfType,
                    duration: temper_authz::DurationScope::Always,
                    agent_type_value: grouped.agent_type.clone(),
                    role_value: None,
                    session_id: None,
                };
                let preview = temper_authz::generate_cedar_from_matrix(
                    grouped.agent_type.as_deref().unwrap_or("*"),
                    "*",
                    &grouped.resource_type,
                    "*",
                    &matrix,
                );
                suggestions.push(PolicySuggestion {
                    description: format!(
                        "{} actions on {} denied {} times for {}",
                        grouped.denied_actions.len(),
                        grouped.resource_type,
                        grouped.total_denials,
                        grouped.agent_type.as_deref().unwrap_or("all agents"),
                    ),
                    matrix,
                    cedar_preview: preview,
                    denial_count: grouped.total_denials,
                    grouped: true,
                });
            }
        }

        // Add per-action suggestions for patterns NOT covered by a grouped suggestion.
        for (key, pattern) in &self.per_action {
            if pattern.count < self.action_threshold {
                continue;
            }
            let type_key = (key.0.clone(), key.2.clone());
            if grouped_keys.contains(&type_key) {
                continue; // Covered by grouped suggestion
            }
            let matrix = temper_authz::PolicyScopeMatrix {
                principal: if pattern.agent_type.is_some() {
                    temper_authz::PrincipalScope::AgentsOfType
                } else {
                    temper_authz::PrincipalScope::AnyAgent
                },
                action: temper_authz::ActionScope::ThisAction,
                resource: temper_authz::ResourceScope::AnyOfType,
                duration: temper_authz::DurationScope::Always,
                agent_type_value: pattern.agent_type.clone(),
                role_value: None,
                session_id: None,
            };
            let preview = temper_authz::generate_cedar_from_matrix(
                pattern.agent_type.as_deref().unwrap_or("*"),
                &pattern.action,
                &pattern.resource_type,
                "*",
                &matrix,
            );
            suggestions.push(PolicySuggestion {
                description: format!(
                    "{} denied {} times on {} for {}",
                    pattern.action,
                    pattern.count,
                    pattern.resource_type,
                    pattern.agent_type.as_deref().unwrap_or("all agents"),
                ),
                matrix,
                cedar_preview: preview,
                denial_count: pattern.count,
                grouped: false,
            });
        }

        suggestions
    }
}

impl Default for PolicySuggestionEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_suggestions_below_threshold() {
        let mut engine = PolicySuggestionEngine::new();
        engine.record_denial(
            Some("claude-code"),
            "read",
            "Doc",
            "doc-1",
            "2025-01-01T00:00:00Z",
        );
        engine.record_denial(
            Some("claude-code"),
            "read",
            "Doc",
            "doc-2",
            "2025-01-01T00:01:00Z",
        );
        assert!(engine.suggestions().is_empty());
    }

    #[test]
    fn per_action_suggestion_at_threshold() {
        let mut engine = PolicySuggestionEngine::new();
        for i in 0..3 {
            engine.record_denial(
                Some("claude-code"),
                "read",
                "Doc",
                &format!("doc-{i}"),
                "2025-01-01T00:00:00Z",
            );
        }
        let suggestions = engine.suggestions();
        assert_eq!(suggestions.len(), 1);
        assert!(!suggestions[0].grouped);
        assert!(suggestions[0].cedar_preview.contains("Action::\"read\""));
    }

    #[test]
    fn grouped_suggestion_replaces_individual() {
        let mut engine = PolicySuggestionEngine::new();
        for action in &["read", "write", "delete"] {
            for i in 0..3 {
                engine.record_denial(
                    Some("claude-code"),
                    action,
                    "Doc",
                    &format!("doc-{i}"),
                    "2025-01-01T00:00:00Z",
                );
            }
        }
        let suggestions = engine.suggestions();
        // Should be 1 grouped suggestion, not 3 individual ones.
        assert_eq!(suggestions.len(), 1);
        assert!(suggestions[0].grouped);
    }

    #[test]
    fn mixed_grouped_and_individual() {
        let mut engine = PolicySuggestionEngine::new();
        // 3 actions on Doc -> grouped
        for action in &["read", "write", "delete"] {
            for i in 0..3 {
                engine.record_denial(Some("claude-code"), action, "Doc", &format!("doc-{i}"), "t");
            }
        }
        // 1 action on Order -> individual (if at threshold)
        for i in 0..3 {
            engine.record_denial(
                Some("claude-code"),
                "submit",
                "Order",
                &format!("ord-{i}"),
                "t",
            );
        }
        let suggestions = engine.suggestions();
        assert_eq!(suggestions.len(), 2);
        assert!(suggestions.iter().any(|s| s.grouped));
        assert!(suggestions.iter().any(|s| !s.grouped));
    }

    #[test]
    fn no_agent_type_suggestions() {
        let mut engine = PolicySuggestionEngine::new();
        for i in 0..3 {
            engine.record_denial(None, "read", "Doc", &format!("doc-{i}"), "t");
        }
        let suggestions = engine.suggestions();
        assert_eq!(suggestions.len(), 1);
        assert!(suggestions[0].description.contains("all agents"));
    }
}
