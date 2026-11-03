//! Resolve spec-declared trusted inputs before action validation and persistence.
use serde_json::Value;
use temper_jit::table::TransitionTable;
use temper_runtime::tenant::TenantId;
use temper_spec::automaton::ParameterSource;

use super::DispatchError;
use crate::{ServerState, request_context::AgentContext};

impl ServerState {
    /// Bind declared parameters from authenticated authority. The caller's JSON
    /// never establishes identity, including when it supplies the correct value.
    pub(crate) fn resolve_authenticated_params(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        action: &str,
        params: Value,
        agent: &AgentContext,
    ) -> Result<Value, DispatchError> {
        let table = self.transition_table_for_dispatch(tenant, entity_type)?;
        bind_authenticated_params(&table, action, params, agent)
    }
}

/// The refusal of a move whose caller is no one a credential stands for.
const NO_CONTEXT: &str = "Authenticated parameter requires a security context";
const NO_CALLER: &str = "Authenticated subject requires a caller identity";

/// Whether `error` refused a move because nothing names its caller, as opposed
/// to something its caller did.
pub(super) fn refused_for_no_caller(error: &DispatchError) -> bool {
    matches!(error, DispatchError::AuthzDenied(reason) if reason == NO_CONTEXT || reason == NO_CALLER)
}

/// [`ServerState::resolve_authenticated_params`] under `table`: a version not
/// yet installed binds a move's parameters as it will once it serves.
pub(crate) fn bind_authenticated_params(
    table: &TransitionTable,
    action: &str,
    mut params: Value,
    agent: &AgentContext,
) -> Result<Value, DispatchError> {
    let action = action.rsplit('.').next().unwrap_or(action);
    let Some(contract) = table.action_contracts.get(action) else {
        return Ok(params);
    };
    if contract.param_sources.is_empty() {
        return Ok(params);
    }
    let fields = params
        .as_object_mut()
        .ok_or_else(|| DispatchError::Internal("Action parameters must be a JSON object".into()))?;
    for (name, source) in &contract.param_sources {
        if fields.contains_key(name) {
            return Err(DispatchError::AuthzDenied(format!(
                "Parameter '{name}' is bound by the runtime and cannot be supplied"
            )));
        }
        let context = agent
            .security_ctx
            .as_ref()
            .ok_or_else(|| DispatchError::AuthzDenied(NO_CONTEXT.into()))?;
        if !crate::request_context::names_a_caller(context) {
            return Err(DispatchError::AuthzDenied(NO_CALLER.into()));
        }
        let principal = &context.principal;
        let value = match source {
            ParameterSource::AuthenticatedSubject => {
                let subject = principal.acting_for.as_deref().unwrap_or(&principal.id);
                if subject.is_empty() {
                    return Err(DispatchError::AuthzDenied(
                        "Authenticated subject is empty".into(),
                    ));
                }
                subject
            }
            ParameterSource::AuthenticatedAgentLabel => {
                principal.label.as_deref().unwrap_or_default()
            }
        };
        fields.insert(name.clone(), Value::String(value.to_owned()));
    }
    Ok(params)
}
