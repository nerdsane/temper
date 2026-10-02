//! Request-local authorization boundary for a prepared operation.

use std::collections::BTreeMap;

use temper_authz::{AuthenticatedRequestContext, AuthzDenial};

use crate::state::ServerState;

/// One exact Cedar check for a prepared command.
pub(crate) struct OperationCheck {
    pub action: String,
    pub resource_type: String,
    pub resource_id: String,
    pub resource_attrs: BTreeMap<String, serde_json::Value>,
}

/// Command and required checks, fixed before authorization.
pub(crate) struct PreparedOperation<T> {
    command: T,
    checks: Vec<OperationCheck>,
}

/// A request-local permit that carries the exact checked command.
pub(crate) struct AuthorizedOperation<T> {
    command: T,
}

#[derive(Debug)]
pub(crate) enum OperationAuthorizationError {
    MissingChecks,
    Denied(AuthzDenial),
}

impl std::fmt::Display for OperationAuthorizationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingChecks => write!(f, "protected operation has no Cedar checks"),
            Self::Denied(denial) => write!(f, "{denial}"),
        }
    }
}

impl<T> PreparedOperation<T> {
    pub fn new(command: T, checks: Vec<OperationCheck>) -> Self {
        Self { command, checks }
    }

    pub fn authorize(
        self,
        state: &ServerState,
        authenticated: &AuthenticatedRequestContext,
    ) -> Result<AuthorizedOperation<T>, OperationAuthorizationError> {
        if self.checks.is_empty() {
            return Err(OperationAuthorizationError::MissingChecks);
        }
        for mut check in self.checks {
            check.resource_attrs.insert(
                "id".to_string(),
                serde_json::Value::String(check.resource_id),
            );
            state
                .authorize_with_context(
                    authenticated.security_context(),
                    &check.action,
                    &check.resource_type,
                    &check.resource_attrs,
                    authenticated.tenant().as_str(),
                )
                .map_err(OperationAuthorizationError::Denied)?;
        }
        Ok(AuthorizedOperation {
            command: self.command,
        })
    }
}

impl<T> AuthorizedOperation<T> {
    pub fn into_command(self) -> T {
        self.command
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use temper_authz::SecurityContext;
    use temper_runtime::tenant::TenantId;

    #[test]
    fn protected_operation_without_checks_fails_closed() {
        let state = ServerState::from_registry(
            temper_runtime::ActorSystem::new("missing-operation-check"),
            crate::registry::SpecRegistry::new(),
        );
        let authenticated = AuthenticatedRequestContext::new(
            TenantId::default(),
            SecurityContext::from_resolved_identity("agent-1", "worker", None),
        );
        let result = PreparedOperation::new("command", vec![]).authorize(&state, &authenticated);
        assert!(matches!(
            result,
            Err(OperationAuthorizationError::MissingChecks)
        ));
    }
}
