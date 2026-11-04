//! What a request leaves of a policy.

use serde_json::Value as Json;

use super::super::json::single;
use super::{ERROR, FALSE, Request, Results, TRUE};

impl Request<'_> {
    /// What `policy`'s action and resource scope say of this question,
    /// which no sender changes.
    pub(in crate::engine::may) fn fixed_scope(&self, policy: &Json) -> (Results, Results) {
        (
            self.action_scope(&policy["action"]),
            self.resource_scope(&policy["resource"]),
        )
    }

    /// `policy` as this request leaves it: its scope's results, and each
    /// condition with every part this request decides replaced by its
    /// result. None when a part it leaves open reads something `reads`
    /// says: two requests that differ only there and leave `policy` the same
    /// answer alike however the rest comes to be known.
    pub(in crate::engine::may) fn residual(
        &self,
        policy: &Json,
        reads: &dyn Fn(&Json) -> bool,
    ) -> Option<Json> {
        let applies = self.applies(policy);
        if let Some(decided) = decided(applies) {
            return Some(decided);
        }
        let object = policy.as_object()?;
        let scope = self
            .principal_scope(&object["principal"])
            .and_then(|| self.action_scope(&object["action"]))
            .and_then(|| self.resource_scope(&object["resource"]));
        let mut parts = vec![Json::from(scope.0)];
        for condition in object.get("conditions").and_then(Json::as_array)? {
            parts.push(condition["kind"].clone());
            parts.push(self.residual_of(&condition["body"], reads)?);
        }
        Some(Json::Array(parts))
    }

    fn residual_of(&self, expr: &Json, reads: &dyn Fn(&Json) -> bool) -> Option<Json> {
        if let Some(decided) = decided(self.truth(expr)) {
            return Some(decided);
        }
        match single(expr) {
            Some((op @ ("&&" | "||"), arg)) => Some(serde_json::json!({ op: [
                self.residual_of(&arg["left"], reads)?,
                self.residual_of(&arg["right"], reads)?,
            ]})),
            Some(("!", arg)) => {
                Some(serde_json::json!({ "!": self.residual_of(&arg["arg"], reads)? }))
            }
            Some(("if-then-else", arg)) => Some(serde_json::json!({ "if-then-else": [
                self.residual_of(&arg["if"], reads)?,
                self.residual_of(&arg["then"], reads)?,
                self.residual_of(&arg["else"], reads)?,
            ]})),
            _ if reads(expr) => None,
            _ => Some(expr.clone()),
        }
    }
}

/// A result every refinement keeps, as a residual holds it.
fn decided(results: Results) -> Option<Json> {
    match results {
        TRUE => Some(Json::from(true)),
        FALSE => Some(Json::from(false)),
        ERROR => Some(Json::from("error")),
        _ => None,
    }
}
