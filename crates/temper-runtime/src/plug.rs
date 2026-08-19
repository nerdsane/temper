//! Control-plane door to a runtime.
//!
//! The server calls [`EntityRuntime::execute`]. It does not name a mailbox
//! message. Each runtime (in-process EntityActor, later Postgres or a
//! Durable Object) implements this trait. See ADR-0167.

use std::collections::BTreeMap;
use std::future::Future;
use std::time::Duration;

/// Work the control plane asks a runtime to do.
#[derive(Debug, Clone, PartialEq)]
pub enum RuntimeRequest {
    /// Run a spec action on the entity.
    Action {
        /// Action name (e.g. `"SubmitOrder"`).
        name: String,
        /// Action parameters.
        params: serde_json::Value,
        /// Pre-resolved cross-entity guard booleans.
        cross_entity_booleans: BTreeMap<String, bool>,
        /// Dedup key for a retried dispatch.
        idempotency_key: Option<String>,
        /// Digest of the state Cedar authorized. Internal calls omit it.
        expected_authorization_precondition: Option<String>,
    },
    /// Read the current entity state.
    GetState,
    /// Read one field.
    GetField {
        /// Field name.
        field: String,
    },
    /// PATCH (`replace = false`) or PUT (`replace = true`) fields.
    UpdateFields {
        /// Field payload. Must be a JSON object at the HTTP boundary.
        fields: serde_json::Value,
        /// When true, replace the field bag; otherwise merge.
        replace: bool,
        /// Digest of the state Cedar authorized.
        expected_precondition: Option<String>,
    },
    /// Tombstone the entity.
    Delete {
        /// Digest of the state Cedar authorized. Internal calls omit it.
        expected_authorization_precondition: Option<String>,
    },
}

impl RuntimeRequest {
    /// Build an action request.
    pub fn action(
        name: impl Into<String>,
        params: serde_json::Value,
        cross_entity_booleans: BTreeMap<String, bool>,
        idempotency_key: Option<String>,
        expected_authorization_precondition: Option<String>,
    ) -> Self {
        Self::Action {
            name: name.into(),
            params,
            cross_entity_booleans,
            idempotency_key,
            expected_authorization_precondition,
        }
    }
}

/// Door the control plane uses to talk to a runtime.
///
/// `execute` is one attempt with a timeout. Retry lives on the caller
/// (ADR-0048).
pub trait EntityRuntime: Send + Sync {
    /// Success payload. The in-process adapter uses the server's
    /// `EntityResponse`.
    type Response: Send + 'static;
    /// Failure payload. The in-process adapter uses [`crate::actor::ActorError`].
    type Error: Send + 'static;

    /// Run `request` and wait up to `timeout` for a reply.
    fn execute(
        &self,
        request: RuntimeRequest,
        timeout: Duration,
    ) -> impl Future<Output = Result<Self::Response, Self::Error>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_request_is_cloneable() {
        let request = RuntimeRequest::action(
            "Submit",
            serde_json::json!({"n": 1}),
            BTreeMap::from([("ok".to_string(), true)]),
            Some("k1".to_string()),
            None,
        );
        let clone = request.clone();
        assert_eq!(request, clone);
    }
}
