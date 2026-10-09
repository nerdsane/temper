//! Trusted in-process OData lineage; never constructed from HTTP headers.
use super::local_completion::LocalCompletionWait;
use super::{AgentContext, CallbackBudgetExceeded};

/// Opaque runtime context for a known, logically joined in-process request.
/// Only the kernel can construct it; transport headers cannot supply evidence,
/// capacity or ancestry. Authentication remains a separate request extension.
#[derive(Clone)]
pub struct LocalDispatchContext(AgentContext);

impl LocalDispatchContext {
    pub(crate) fn for_child(parent: &AgentContext) -> Result<Self, CallbackBudgetExceeded> {
        parent.for_callback().map(Self)
    }

    pub(crate) fn apply_to(&self, child: AgentContext) -> AgentContext {
        // This intentionally does NOT copy identity or idempotency_key.
        child.inherit_observability_from(&self.0)
    }

    pub(crate) fn observe_wait(&self) -> LocalCompletionWait {
        self.0.local_completion.observe_wait()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_context_carries_budgets_but_not_keys_or_authority() {
        let parent = AgentContext {
            idempotency_key: Some("parent-key".into()),
            callback_depth: 2,
            callback_hops: 5,
            effects_ancestors: vec![("tenant:Parent:one".into(), "parent-key".into())],
            ..AgentContext::system()
        };
        let child = AgentContext {
            idempotency_key: Some("child-key".into()),
            ..AgentContext::for_service("child")
        };
        let expected_identity = child.agent_id.clone();
        let local = LocalDispatchContext::for_child(&parent).unwrap();
        let carried = local.apply_to(child);
        assert_eq!(carried.agent_id, expected_identity);
        assert_eq!(carried.idempotency_key.as_deref(), Some("child-key"));
        assert_eq!((carried.callback_depth, carried.callback_hops), (3, 6));
        assert_eq!(carried.effects_ancestors, parent.effects_ancestors);
        local.observe_wait().joined();
        assert!(!parent.local_completion.is_unknown());
        drop(local.observe_wait());
        assert!(parent.local_completion.is_unknown());
    }
}
