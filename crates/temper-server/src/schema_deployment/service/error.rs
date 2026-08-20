use super::*;

#[derive(Debug)]
pub(crate) struct ServiceError {
    pub(super) response: SchemaDeploymentErrorV1,
}

impl ServiceError {
    pub(super) fn code(&self) -> &str {
        &self.response.code
    }

    pub(crate) fn message(&self) -> &str {
        &self.response.message
    }

    pub(super) fn is_retryable(&self) -> bool {
        self.response.retryable
    }

    pub(super) fn new(code: &str, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            response: SchemaDeploymentErrorV1 {
                code: code.into(),
                message: message.into(),
                retryable,
                decision_id: None,
            },
        }
    }

    pub(super) fn authorization(decision_id: String) -> Self {
        Self {
            response: SchemaDeploymentErrorV1 {
                code: "authorization_denied".into(),
                message: "Cedar denied the schema deployment operation".into(),
                retryable: false,
                decision_id: Some(decision_id),
            },
        }
    }

    pub(super) fn from_store(error: SchemaDeploymentStoreError) -> Self {
        match error {
            SchemaDeploymentStoreError::InvalidInput(message) => {
                Self::new("invalid_bundle", message, false)
            }
            SchemaDeploymentStoreError::IdempotencyConflict => {
                Self::new("idempotency_conflict", error.to_string(), false)
            }
            SchemaDeploymentStoreError::NotFound => {
                Self::new("invalid_bundle", error.to_string(), false)
            }
            SchemaDeploymentStoreError::InvalidLifecycleTransition => {
                Self::new("invalid_lifecycle_transition", error.to_string(), true)
            }
            SchemaDeploymentStoreError::PredecessorMismatch => {
                Self::new("predecessor_mismatch", error.to_string(), true)
            }
            SchemaDeploymentStoreError::StaleFence => {
                Self::new("stale_fence", error.to_string(), true)
            }
            SchemaDeploymentStoreError::VerificationFailed => {
                Self::new("verification_failed", error.to_string(), false)
            }
            SchemaDeploymentStoreError::MigrationBudgetExhausted => {
                Self::new("migration_budget_exhausted", error.to_string(), false)
            }
            SchemaDeploymentStoreError::MigrationRejected => {
                Self::new("migration_rejected", error.to_string(), false)
            }
            SchemaDeploymentStoreError::BackendUnavailable(message) => {
                Self::new("backend_unavailable", message, true)
            }
        }
    }

    pub(super) fn from_migration(error: temper_wasm::PureMigrationError) -> Self {
        match error {
            temper_wasm::PureMigrationError::Rejected(message) => {
                Self::new("migration_rejected", message, false)
            }
            temper_wasm::PureMigrationError::BudgetExhausted(message) => {
                Self::new("migration_budget_exhausted", message, false)
            }
            temper_wasm::PureMigrationError::Failed(message)
            | temper_wasm::PureMigrationError::ModuleNotFound(message) => {
                Self::new("migration_failed", message, true)
            }
        }
    }

    pub(super) fn status(&self) -> StatusCode {
        match self.response.code.as_str() {
            "authorization_denied" => StatusCode::FORBIDDEN,
            "invalid_bundle" | "scope_mismatch" | "digest_mismatch" => StatusCode::BAD_REQUEST,
            "verification_failed" | "migration_budget_exhausted" | "migration_rejected" => {
                StatusCode::UNPROCESSABLE_ENTITY
            }
            "migration_failed" => StatusCode::INTERNAL_SERVER_ERROR,
            "backend_unavailable" => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::CONFLICT,
        }
    }

    pub(crate) fn into_contract(self) -> SchemaDeploymentErrorV1 {
        self.response
    }
}
