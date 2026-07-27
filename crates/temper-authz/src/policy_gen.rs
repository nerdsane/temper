//! Cedar policy generation from a multi-dimensional scope matrix.
//!
//! Replaces the old Narrow/Medium/Broad enum with a composable matrix of
//! principal × action × resource × duration scopes. Each dimension is
//! independently selectable, giving fine-grained control over generated Cedar
//! policies.

use std::str::FromStr;

use cedar_policy::{EntityId, EntityTypeName, EntityUid};
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

/// Render a Cedar entity UID (`Type::"id"`).
///
/// The type name is validated by Cedar (`EntityTypeName::from_str`) and the id
/// is escaped by Cedar itself (`EntityUid`'s `Display`), so an agent-influenced
/// id can never break out of its string literal into policy structure. A type
/// name that is not a valid Cedar identifier fails closed with an error rather
/// than producing a malformed or wider-than-approved policy (ARN-172).
fn cedar_uid(type_name: &str, id: &str) -> Result<String, String> {
    let ty = EntityTypeName::from_str(type_name)
        .map_err(|e| format!("invalid entity type name {type_name:?}: {e}"))?;
    Ok(EntityUid::from_type_name_and_id(ty, EntityId::new(id)).to_string())
}

/// Validate and render a bare Cedar entity type name (for `principal is Type`
/// / `resource is Type`). Fails closed on a non-identifier type name.
pub fn render_cedar_entity_type(type_name: &str) -> Result<String, String> {
    EntityTypeName::from_str(type_name)
        .map(|ty| ty.to_string())
        .map_err(|e| format!("invalid entity type name {type_name:?}: {e}"))
}

/// Render a Cedar string literal, escaping the value with Cedar's own routine
/// (the same escaping `EntityUid`'s `Display` uses via `Eid::escaped`). This
/// confines the value to the literal so it cannot inject additional clauses or
/// policies (ARN-172).
fn cedar_string_literal(value: &str) -> String {
    format!("\"{}\"", EntityId::new(value).escaped())
}

/// Generate a Cedar permit statement from a scope matrix.
///
/// Each matrix dimension maps to a specific Cedar clause:
/// - **PrincipalScope**: principal clause
/// - **ActionScope**: action clause
/// - **ResourceScope**: resource clause
/// - **DurationScope**: optional `when` condition for session scoping
///
/// All agent-influenced values (`agent_id`, `action`, `resource_id`, `role`,
/// `agent_type`, `session_id`) are confined to Cedar string literals, and all
/// type-name positions (`principal_kind`, `resource_type`) are validated as
/// Cedar identifiers. Returns an error (fails closed) if a type-name position
/// is not a valid Cedar entity type, so a crafted value can neither break the
/// tenant policy reload nor widen the approved scope (ARN-172).
pub fn generate_cedar_from_matrix(
    agent_id: &str,
    principal_kind: &str,
    action: &str,
    resource_type: &str,
    resource_id: &str,
    matrix: &PolicyScopeMatrix,
) -> Result<String, String> {
    // This function is the policy-generation security boundary. Validate here
    // rather than relying on callers or debug-only assertions: a missing
    // companion value would otherwise silently widen the generated policy in
    // release builds.
    validate_policy_scope_matrix(matrix)?;

    let principal_clause = match &matrix.principal {
        PrincipalScope::ThisAgent => {
            format!("principal == {}", cedar_uid(principal_kind, agent_id)?)
        }
        PrincipalScope::AgentsWithRole
        | PrincipalScope::AgentsOfType
        | PrincipalScope::AnyAgent => {
            format!("principal is {}", render_cedar_entity_type(principal_kind)?)
        }
    };

    let action_clause = match &matrix.action {
        ActionScope::ThisAction => format!("action == {}", cedar_uid("Action", action)?),
        ActionScope::AllActionsOnType | ActionScope::AllActions => "action".to_string(),
    };

    let resource_clause = match (&matrix.action, &matrix.resource) {
        (_, ResourceScope::ThisResource) => {
            format!("resource == {}", cedar_uid(resource_type, resource_id)?)
        }
        (_, ResourceScope::AnyOfType)
        | (ActionScope::AllActionsOnType, ResourceScope::AnyResource) => {
            format!("resource is {}", render_cedar_entity_type(resource_type)?)
        }
        (_, ResourceScope::AnyResource) => "resource".to_string(),
    };

    // Build when conditions.
    let mut conditions: Vec<String> = Vec::new();

    match &matrix.principal {
        PrincipalScope::AgentsWithRole => {
            if let Some(ref role) = matrix.role_value {
                conditions.push(format!("principal.role == {}", cedar_string_literal(role)));
                conditions.push("principal.agentTypeVerified == true".to_string());
            }
        }
        PrincipalScope::AgentsOfType => {
            if let Some(ref agent_type) = matrix.agent_type_value {
                conditions.push(format!(
                    "principal.agent_type == {}",
                    cedar_string_literal(agent_type)
                ));
                // Require credential-verified identity (ADR-0033).
                conditions.push("principal.agentTypeVerified == true".to_string());
            }
        }
        _ => {}
    }

    if matrix.duration == DurationScope::Session
        && let Some(ref session_id) = matrix.session_id
    {
        conditions.push(format!(
            "context.sessionId == {}",
            cedar_string_literal(session_id)
        ));
    }

    let when_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("\nwhen {{ {} }}", conditions.join(" && "))
    };

    Ok(format!(
        "permit(\n  {},\n  {},\n  {}\n){};",
        principal_clause, action_clause, resource_clause, when_clause,
    ))
}

#[cfg(test)]
#[path = "policy_gen_test.rs"]
mod policy_gen_test;
