//! Cedar policy generation from a multi-dimensional scope matrix.
//!
//! Replaces the old Narrow/Medium/Broad enum with a composable matrix of
//! principal × action × resource × duration scopes. Each dimension is
//! independently selectable, giving fine-grained control over generated Cedar
//! policies.

use serde::{Deserialize, Serialize};

/// Who the policy applies to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrincipalScope {
    /// Only the specific agent that was denied.
    ThisAgent,
    /// All agents sharing a particular role.
    AgentsWithRole,
    /// All agents of a specific type (e.g. "claude-code").
    AgentsOfType,
    /// Any authenticated agent.
    AnyAgent,
}

/// Which actions the policy covers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionScope {
    /// Only the specific denied action.
    ThisAction,
    /// All actions on the specified resource type.
    AllActionsOnType,
    /// All actions on any resource.
    AllActions,
}

/// Which resources the policy covers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceScope {
    /// Only the exact resource that was denied.
    ThisResource,
    /// Any resource of the same type.
    AnyOfType,
    /// Any resource of any type.
    AnyResource,
}

/// How long the policy lasts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DurationScope {
    /// Scoped to a specific session (adds sessionId condition).
    Session,
    /// Permanent policy.
    Always,
}

/// Multi-dimensional policy scope matrix.
///
/// Each dimension is independently selectable. The matrix is serialized as JSON
/// and stored on approved `PendingDecision` records.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyScopeMatrix {
    /// Who the policy applies to.
    pub principal: PrincipalScope,
    /// Which actions are covered.
    pub action: ActionScope,
    /// Which resources are covered.
    pub resource: ResourceScope,
    /// How long the policy lasts.
    pub duration: DurationScope,
    /// Required when `principal == AgentsOfType`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type_value: Option<String>,
    /// Required when `principal == AgentsWithRole`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_value: Option<String>,
    /// Required when `duration == Session`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

impl PolicyScopeMatrix {
    /// Sensible default: ThisAgent + ThisAction + AnyOfType + Always.
    ///
    /// Equivalent to the old "medium" scope. If `agent_type` is provided,
    /// stores it for potential use with `AgentsOfType`.
    pub fn default_for(agent_type: Option<&str>) -> Self {
        Self {
            principal: PrincipalScope::ThisAgent,
            action: ActionScope::ThisAction,
            resource: ResourceScope::AnyOfType,
            duration: DurationScope::Always,
            agent_type_value: agent_type.map(String::from),
            role_value: None,
            session_id: None,
        }
    }
}

/// Generate a Cedar permit statement from a scope matrix.
///
/// Each matrix dimension maps to a specific Cedar clause:
/// - **PrincipalScope**: principal clause
/// - **ActionScope**: action clause
/// - **ResourceScope**: resource clause
/// - **DurationScope**: optional `when` condition for session scoping
pub fn generate_cedar_from_matrix(
    agent_id: &str,
    action: &str,
    resource_type: &str,
    resource_id: &str,
    matrix: &PolicyScopeMatrix,
) -> String {
    // Pre-assertions (TigerStyle): companion fields must be present when their scope requires them.
    debug_assert!(
        matrix.principal != PrincipalScope::AgentsOfType || matrix.agent_type_value.is_some(),
        "AgentsOfType requires agent_type_value"
    );
    debug_assert!(
        matrix.principal != PrincipalScope::AgentsWithRole || matrix.role_value.is_some(),
        "AgentsWithRole requires role_value"
    );
    debug_assert!(
        matrix.duration != DurationScope::Session || matrix.session_id.is_some(),
        "Session duration requires session_id"
    );

    let principal_clause = match &matrix.principal {
        PrincipalScope::ThisAgent => format!("principal == Agent::\"{}\"", agent_id),
        PrincipalScope::AgentsWithRole
        | PrincipalScope::AgentsOfType
        | PrincipalScope::AnyAgent => "principal is Agent".to_string(),
    };

    let action_clause = match &matrix.action {
        ActionScope::ThisAction => format!("action == Action::\"{}\"", action),
        ActionScope::AllActionsOnType | ActionScope::AllActions => "action".to_string(),
    };

    let resource_clause = match &matrix.resource {
        ResourceScope::ThisResource => {
            format!("resource == {}::\"{}\"", resource_type, resource_id)
        }
        ResourceScope::AnyOfType => format!("resource is {}", resource_type),
        ResourceScope::AnyResource => "resource".to_string(),
    };

    // Build when conditions.
    let mut conditions: Vec<String> = Vec::new();

    match &matrix.principal {
        PrincipalScope::AgentsWithRole => {
            if let Some(ref role) = matrix.role_value {
                conditions.push(format!("context.role == \"{}\"", role));
            }
        }
        PrincipalScope::AgentsOfType => {
            if let Some(ref agent_type) = matrix.agent_type_value {
                conditions.push(format!("context.agentType == \"{}\"", agent_type));
            }
        }
        _ => {}
    }

    if matrix.duration == DurationScope::Session {
        if let Some(ref session_id) = matrix.session_id {
            conditions.push(format!("context.sessionId == \"{}\"", session_id));
        }
    }

    let when_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("\nwhen {{ {} }}", conditions.join(" && "))
    };

    format!(
        "permit(\n  {},\n  {},\n  {}\n){};",
        principal_clause, action_clause, resource_clause, when_clause,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_this_agent_this_action_this_resource() {
        let m = PolicyScopeMatrix {
            principal: PrincipalScope::ThisAgent,
            action: ActionScope::ThisAction,
            resource: ResourceScope::ThisResource,
            duration: DurationScope::Always,
            agent_type_value: None,
            role_value: None,
            session_id: None,
        };
        let policy = generate_cedar_from_matrix("bot-1", "submitOrder", "Order", "order-123", &m);
        assert!(policy.contains("principal == Agent::\"bot-1\""));
        assert!(policy.contains("action == Action::\"submitOrder\""));
        assert!(policy.contains("resource == Order::\"order-123\""));
        assert!(!policy.contains("when"));
    }

    #[test]
    fn test_this_agent_this_action_any_of_type() {
        let m = PolicyScopeMatrix {
            principal: PrincipalScope::ThisAgent,
            action: ActionScope::ThisAction,
            resource: ResourceScope::AnyOfType,
            duration: DurationScope::Always,
            agent_type_value: None,
            role_value: None,
            session_id: None,
        };
        let policy = generate_cedar_from_matrix("bot-1", "submitOrder", "Order", "order-123", &m);
        assert!(policy.contains("resource is Order"));
    }

    #[test]
    fn test_any_agent_all_actions_any_resource() {
        let m = PolicyScopeMatrix {
            principal: PrincipalScope::AnyAgent,
            action: ActionScope::AllActions,
            resource: ResourceScope::AnyResource,
            duration: DurationScope::Always,
            agent_type_value: None,
            role_value: None,
            session_id: None,
        };
        let policy = generate_cedar_from_matrix("bot-1", "submitOrder", "Order", "order-123", &m);
        assert!(policy.contains("principal is Agent"));
        assert!(!policy.contains("Action::"));
        assert!(!policy.contains("Order"));
    }

    #[test]
    fn test_agents_of_type_condition() {
        let m = PolicyScopeMatrix {
            principal: PrincipalScope::AgentsOfType,
            action: ActionScope::ThisAction,
            resource: ResourceScope::AnyOfType,
            duration: DurationScope::Always,
            agent_type_value: Some("claude-code".to_string()),
            role_value: None,
            session_id: None,
        };
        let policy = generate_cedar_from_matrix("bot-1", "submitOrder", "Order", "order-123", &m);
        assert!(policy.contains("principal is Agent"));
        assert!(policy.contains("context.agentType == \"claude-code\""));
    }

    #[test]
    fn test_agents_with_role_condition() {
        let m = PolicyScopeMatrix {
            principal: PrincipalScope::AgentsWithRole,
            action: ActionScope::ThisAction,
            resource: ResourceScope::AnyOfType,
            duration: DurationScope::Always,
            agent_type_value: None,
            role_value: Some("operations_agent".to_string()),
            session_id: None,
        };
        let policy = generate_cedar_from_matrix("bot-1", "submitOrder", "Order", "order-123", &m);
        assert!(policy.contains("context.role == \"operations_agent\""));
    }

    #[test]
    fn test_session_duration_adds_session_id() {
        let m = PolicyScopeMatrix {
            principal: PrincipalScope::ThisAgent,
            action: ActionScope::ThisAction,
            resource: ResourceScope::AnyOfType,
            duration: DurationScope::Session,
            agent_type_value: None,
            role_value: None,
            session_id: Some("sess-abc".to_string()),
        };
        let policy = generate_cedar_from_matrix("bot-1", "submitOrder", "Order", "order-123", &m);
        assert!(policy.contains("context.sessionId == \"sess-abc\""));
    }

    #[test]
    fn test_combined_agent_type_and_session() {
        let m = PolicyScopeMatrix {
            principal: PrincipalScope::AgentsOfType,
            action: ActionScope::ThisAction,
            resource: ResourceScope::AnyOfType,
            duration: DurationScope::Session,
            agent_type_value: Some("openclaw".to_string()),
            role_value: None,
            session_id: Some("sess-xyz".to_string()),
        };
        let policy = generate_cedar_from_matrix("bot-1", "submitOrder", "Order", "order-123", &m);
        assert!(policy.contains("context.agentType == \"openclaw\""));
        assert!(policy.contains("context.sessionId == \"sess-xyz\""));
    }

    #[test]
    fn test_all_actions_on_type_still_constrains_resource() {
        let m = PolicyScopeMatrix {
            principal: PrincipalScope::ThisAgent,
            action: ActionScope::AllActionsOnType,
            resource: ResourceScope::AnyOfType,
            duration: DurationScope::Always,
            agent_type_value: None,
            role_value: None,
            session_id: None,
        };
        let policy = generate_cedar_from_matrix("bot-1", "submitOrder", "Order", "order-123", &m);
        assert!(policy.contains("resource is Order"));
        assert!(!policy.contains("Action::"));
    }

    #[test]
    fn test_default_matrix() {
        let m = PolicyScopeMatrix::default_for(Some("claude-code"));
        assert_eq!(m.principal, PrincipalScope::ThisAgent);
        assert_eq!(m.action, ActionScope::ThisAction);
        assert_eq!(m.resource, ResourceScope::AnyOfType);
        assert_eq!(m.duration, DurationScope::Always);
        assert_eq!(m.agent_type_value, Some("claude-code".to_string()));
    }
}
