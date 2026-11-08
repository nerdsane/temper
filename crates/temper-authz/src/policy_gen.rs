//! Cedar policy generation from a multi-dimensional scope matrix.
//!
//! Replaces the old Narrow/Medium/Broad enum with a composable matrix of
//! principal × action × resource × duration scopes. Each dimension is
//! independently selectable, giving fine-grained control over generated Cedar
//! policies.

use std::str::FromStr;

use cedar_policy::{EntityId, EntityTypeName, EntityUid, PolicySet};
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

/// Validate that a scope matrix is internally consistent.
///
/// Returns an error if required companion fields are missing or empty:
/// - `principal = AgentsOfType` requires non-empty `agent_type_value`
/// - `principal = AgentsWithRole` requires non-empty `role_value`
/// - `duration = Session` requires non-empty `session_id`
pub fn validate_policy_scope_matrix(matrix: &PolicyScopeMatrix) -> Result<(), String> {
    if matrix.principal == PrincipalScope::AgentsOfType {
        let Some(agent_type) = matrix.agent_type_value.as_deref() else {
            return Err("principal=agents_of_type requires agent_type_value".to_string());
        };
        if agent_type.trim().is_empty() {
            return Err("principal=agents_of_type requires non-empty agent_type_value".to_string());
        }
    }

    if matrix.principal == PrincipalScope::AgentsWithRole {
        let Some(role) = matrix.role_value.as_deref() else {
            return Err("principal=agents_with_role requires role_value".to_string());
        };
        if role.trim().is_empty() {
            return Err("principal=agents_with_role requires non-empty role_value".to_string());
        }
    }

    if matrix.duration == DurationScope::Session {
        let Some(session_id) = matrix.session_id.as_deref() else {
            return Err("duration=session requires session_id".to_string());
        };
        if session_id.trim().is_empty() {
            return Err("duration=session requires non-empty session_id".to_string());
        }
    }

    Ok(())
}

/// Generate a Cedar permit statement from a scope matrix.
///
/// Each matrix dimension maps to a specific Cedar clause:
/// - **PrincipalScope**: principal clause
/// - **ActionScope**: action clause
/// - **ResourceScope**: resource clause
/// - **DurationScope**: optional `when` condition for session scoping
///
/// Every value is rendered as a typed Cedar value: entity types must be valid
/// Cedar type names, and entity ids and condition values become escaped Cedar
/// string literals. The result is always exactly one permit statement with the
/// matrix's constraints; a value that cannot be rendered that way is an error
/// and no text is produced.
pub fn generate_cedar_from_matrix(
    agent_id: &str,
    principal_kind: &str,
    action: &str,
    resource_type: &str,
    resource_id: &str,
    matrix: &PolicyScopeMatrix,
) -> Result<String, String> {
    // Precondition: companion fields are present when their scope requires them.
    validate_policy_scope_matrix(matrix)?;

    let principal_clause = match &matrix.principal {
        PrincipalScope::ThisAgent => {
            format!("principal == {}", entity_uid(principal_kind, agent_id)?)
        }
        PrincipalScope::AgentsWithRole
        | PrincipalScope::AgentsOfType
        | PrincipalScope::AnyAgent => format!("principal is {}", entity_type(principal_kind)?),
    };

    let action_clause = match &matrix.action {
        ActionScope::ThisAction => format!("action == {}", entity_uid("Action", action)?),
        ActionScope::AllActionsOnType | ActionScope::AllActions => "action".to_string(),
    };

    let resource_clause = match &matrix.resource {
        ResourceScope::ThisResource => {
            format!("resource == {}", entity_uid(resource_type, resource_id)?)
        }
        ResourceScope::AnyOfType => format!("resource is {}", entity_type(resource_type)?),
        ResourceScope::AnyResource => "resource".to_string(),
    };

    // Build when conditions.
    let mut conditions: Vec<String> = Vec::new();

    match &matrix.principal {
        PrincipalScope::AgentsWithRole => {
            if let Some(ref role) = matrix.role_value {
                conditions.push(format!("context.role == {}", cedar_string(role)));
            }
        }
        PrincipalScope::AgentsOfType => {
            if let Some(ref agent_type) = matrix.agent_type_value {
                conditions.push(format!("context.agentType == {}", cedar_string(agent_type)));
                // Require credential-verified identity (ADR-0033).
                conditions.push("context.agentTypeVerified == true".to_string());
            }
        }
        _ => {}
    }

    if matrix.duration == DurationScope::Session
        && let Some(ref session_id) = matrix.session_id
    {
        conditions.push(format!("context.sessionId == {}", cedar_string(session_id)));
    }

    let when_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("\nwhen {{ {} }}", conditions.join(" && "))
    };

    let text = format!(
        "permit(\n  {},\n  {},\n  {}\n){};",
        principal_clause, action_clause, resource_clause, when_clause,
    );

    // Postcondition: the text is exactly one statement.
    let parsed =
        PolicySet::from_str(&text).map_err(|e| format!("generated Cedar does not parse: {e}"))?;
    let count = parsed.policies().count();
    if count != 1 {
        return Err(format!(
            "generated Cedar must be exactly one statement, got {count}"
        ));
    }
    Ok(text)
}

/// Parse a Cedar entity type name such as `Agent` or `Shop::Order`.
fn entity_type(name: &str) -> Result<EntityTypeName, String> {
    EntityTypeName::from_str(name)
        .map_err(|e| format!("{name:?} is not a Cedar entity type name: {e}"))
}

/// Build an entity UID from a type name and an id. Its `Display` renders the
/// id as an escaped Cedar string literal.
fn entity_uid(type_name: &str, id: &str) -> Result<EntityUid, String> {
    Ok(EntityUid::from_type_name_and_id(
        entity_type(type_name)?,
        EntityId::new(id),
    ))
}

/// Render a Cedar string literal, escaped the way Cedar renders entity ids.
fn cedar_string(value: &str) -> String {
    format!("\"{}\"", value.escape_debug())
}

#[cfg(test)]
#[path = "policy_gen_test.rs"]
mod tests;
