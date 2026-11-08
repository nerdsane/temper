//! What a request leaves of a policy.

use std::fmt::Write as _;

use serde_json::Value as Json;

use super::super::json::{at, single};
use super::{ANY, ERROR, FALSE, Request, Results, TRUE};

impl Request<'_> {
    /// What `policy`'s action and resource scope say of this question,
    /// which no sender changes.
    pub(in crate::engine::may) fn fixed_scope(&self, policy: &Json) -> (Results, Results) {
        (
            self.action_scope(at(policy, "action")),
            self.resource_scope(at(policy, "resource")),
        )
    }

    /// `policy` as this request leaves it, as JSON text: its scope's
    /// results, and each condition with every part this request decides
    /// replaced by its result. Only its sameness is read, so it is written
    /// as it is found, never built as JSON first. None when a part it leaves open reads something `reads`
    /// says: two requests that differ only there and leave `policy` the same
    /// answer alike however the rest comes to be known.
    pub(in crate::engine::may) fn residual(
        &self,
        policy: &Json,
        reads: &dyn Fn(&Json) -> bool,
    ) -> Option<String> {
        let object = policy.as_object()?;
        let scope = self
            .principal_scope(&object["principal"])
            .and_then(|| self.action_scope(&object["action"]))
            .and_then(|| self.resource_scope(&object["resource"]));
        let conditions = object
            .get("conditions")
            .and_then(Json::as_array)
            .map_or(&[][..], Vec::as_slice);
        // As `applies` judges them, keeping each body's parts for the
        // residual below rather than judging them again.
        let mut applies = scope;
        let mut judged = Vec::with_capacity(conditions.len());
        for condition in conditions {
            let mut body = None;
            applies = applies.and_then(|| {
                let found = self.judge(at(condition, "body"));
                let results = found.results;
                body = Some(found);
                match at(condition, "kind").as_str() {
                    Some("when") => results,
                    Some("unless") => results.not(),
                    _ => ANY,
                }
            });
            judged.push(body);
        }
        if let Some(decided) = decided(applies) {
            return Some(decided.to_string());
        }
        // The text of the JSON array [scope, kind, body, kind, body, ...],
        // written as it is read rather than built and then printed.
        let mut out = format!("[{}", scope.0);
        let conditions = object.get("conditions").and_then(Json::as_array)?;
        for (condition, body) in conditions.iter().zip(judged) {
            write!(out, ",{},", at(condition, "kind")).ok()?;
            self.residual_of(at(condition, "body"), body, reads, &mut out)?;
        }
        out.push(']');
        Some(out)
    }

    /// The results of `expr` as [`Self::truth`] finds them, with those of
    /// the parts it judged on the way, which a residual reads.
    fn judge(&self, expr: &Json) -> Judged {
        let judged = |results, parts| Judged { results, parts };
        match single(expr) {
            Some(("&&", arg)) => {
                let left = self.judge(at(arg, "left"));
                let mut right = None;
                let results = left.results.and_then(|| {
                    let found = self.judge(at(arg, "right"));
                    let results = found.results;
                    right = Some(found);
                    results
                });
                judged(results, vec![Some(left), right])
            }
            Some(("||", arg)) => {
                let left = self.judge(at(arg, "left"));
                let mut right = None;
                let results = left.results.or_else(|| {
                    let found = self.judge(at(arg, "right"));
                    let results = found.results;
                    right = Some(found);
                    results
                });
                judged(results, vec![Some(left), right])
            }
            Some(("!", arg)) => {
                let inner = self.judge(at(arg, "arg"));
                judged(inner.results.not(), vec![Some(inner)])
            }
            Some(("if-then-else", arg)) => {
                let test = self.judge(at(arg, "if"));
                let (mut then, mut otherwise) = (None, None);
                let results = test.results.choose(
                    || {
                        let found = self.judge(at(arg, "then"));
                        let results = found.results;
                        then = Some(found);
                        results
                    },
                    || {
                        let found = self.judge(at(arg, "else"));
                        let results = found.results;
                        otherwise = Some(found);
                        results
                    },
                );
                judged(results, vec![Some(test), then, otherwise])
            }
            _ => judged(self.truth(expr), Vec::new()),
        }
    }

    /// Writes `expr` with every part this request decides replaced by its
    /// result, as JSON text, given what [`Self::judge`] found of it, if it
    /// was judged. None when a part left open reads what `reads` says.
    fn residual_of(
        &self,
        expr: &Json,
        judged: Option<Judged>,
        reads: &dyn Fn(&Json) -> bool,
        out: &mut String,
    ) -> Option<()> {
        let judged = judged.unwrap_or_else(|| self.judge(expr));
        if let Some(decided) = decided(judged.results) {
            write!(out, "{decided}").ok()?;
            return Some(());
        }
        let mut parts = judged.parts.into_iter();
        let mut part = |expr: &Json, out: &mut String| {
            self.residual_of(expr, parts.next().flatten(), reads, out)
        };
        match single(expr) {
            Some((op @ ("&&" | "||"), arg)) => {
                write!(out, "{{\"{op}\":[").ok()?;
                part(at(arg, "left"), out)?;
                out.push(',');
                part(at(arg, "right"), out)?;
                out.push_str("]}");
            }
            Some(("!", arg)) => {
                out.push_str("{\"!\":");
                part(at(arg, "arg"), out)?;
                out.push('}');
            }
            Some(("if-then-else", arg)) => {
                out.push_str("{\"if-then-else\":[");
                part(at(arg, "if"), out)?;
                out.push(',');
                part(at(arg, "then"), out)?;
                out.push(',');
                part(at(arg, "else"), out)?;
                out.push_str("]}");
            }
            _ if reads(expr) => return None,
            _ => write!(out, "{expr}").ok()?,
        }
        Some(())
    }
}

/// A condition's results, with those of the parts a residual reads, in
/// order; a part [`Request::truth`] would not judge is `None`.
struct Judged {
    results: Results,
    parts: Vec<Option<Judged>>,
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
