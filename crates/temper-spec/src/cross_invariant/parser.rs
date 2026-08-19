use thiserror::Error;

use super::types::{CrossInvariantSpec, DeletePolicy, InvariantKind};

/// Operator used in a `RelatedFieldAssert`.
///
/// Currently limited to `In` and `NotIn` — comparison operators (`<`, `>=`, …)
/// and null checks are deliberately out of scope. See ADR-0041 for the
/// rationale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CrossInvariantOperator {
    /// `field in ["A", "B"]` — passes when the related entity's field equals
    /// any listed literal.
    In,
    /// `field not in ["A"]` — passes when the related entity's field equals
    /// none of the listed literals.
    NotIn,
}

/// Parsed assertion — the general form used by the extended grammar.
///
/// Supports:
///
/// ```text
/// related(TargetEntity, sourceField).<FieldName> in     [<literal>, ...]
/// related(TargetEntity, sourceField).<FieldName> not in [<literal>, ...]
/// ```
///
/// Where `<FieldName>` defaults to `status` for backwards compatibility with
/// the pre-ADR-0041 form.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RelatedFieldAssert {
    /// Target entity type (e.g. `Environment`).
    pub target_entity: String,
    /// FK field on the current entity holding the target entity's id.
    pub source_field: String,
    /// Scalar field name on the target to compare against. Defaults to
    /// `"status"` for backwards compatibility.
    pub field_name: String,
    /// In-list / not-in-list operator.
    pub operator: CrossInvariantOperator,
    /// Literal values on the right-hand side of the operator.
    pub values: Vec<String>,
}

impl RelatedFieldAssert {
    /// Returns true when this assertion targets the default `status` field
    /// with the `in` operator — the pre-ADR-0041 grammar shape.
    pub fn is_legacy_status_in(&self) -> bool {
        self.field_name == "status" && self.operator == CrossInvariantOperator::In
    }
}

/// Parsed assertion form for legacy callers:
/// `related(TargetEntity, sourceField).status in ["StateA","StateB"]`
///
/// Retained for backwards compatibility — new code should prefer
/// [`RelatedFieldAssert`] and [`parse_related_field_assert`], which handle
/// the extended grammar (arbitrary field name, `not in`) as well.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RelatedStatusInAssert {
    pub target_entity: String,
    pub source_field: String,
    pub statuses: Vec<String>,
}

#[derive(Debug, Error)]
pub enum CrossInvariantParseError {
    #[error("failed to parse cross-invariants TOML: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("invariant '{name}' has invalid trigger '{on}' (expected Entity.* or Entity.Action)")]
    InvalidTrigger { name: String, on: String },
    #[error("invariant '{name}' has unsupported assertion syntax")]
    InvalidAssertionSyntax { name: String },
    #[error("invariant '{name}' is eventual but missing window_ms")]
    MissingEventualWindow { name: String },
    #[error("invariant '{name}' has non-positive window_ms")]
    InvalidEventualWindow { name: String },
    #[error(
        "unsupported delete policy '{policy}' in {location}; only 'restrict' is supported in v1"
    )]
    UnsupportedDeletePolicy { location: String, policy: String },
}

/// Parse `cross-invariants.toml` into a typed spec.
pub fn parse_cross_invariants(
    source: &str,
) -> Result<CrossInvariantSpec, CrossInvariantParseError> {
    let spec: CrossInvariantSpec = toml::from_str(source)?;

    if spec.default_delete_policy != DeletePolicy::Restrict {
        return Err(CrossInvariantParseError::UnsupportedDeletePolicy {
            location: "default_delete_policy".to_string(),
            policy: format!("{:?}", spec.default_delete_policy).to_ascii_lowercase(),
        });
    }
    for ov in &spec.relation_overrides {
        if ov.delete_policy != DeletePolicy::Restrict {
            return Err(CrossInvariantParseError::UnsupportedDeletePolicy {
                location: format!(
                    "relation_override {}.{}",
                    ov.from_entity, ov.navigation_property
                ),
                policy: format!("{:?}", ov.delete_policy).to_ascii_lowercase(),
            });
        }
    }

    for inv in &spec.invariants {
        if split_trigger(&inv.on).is_none() {
            return Err(CrossInvariantParseError::InvalidTrigger {
                name: inv.name.clone(),
                on: inv.on.clone(),
            });
        }
        if parse_related_field_assert(&inv.assertion).is_none() {
            return Err(CrossInvariantParseError::InvalidAssertionSyntax {
                name: inv.name.clone(),
            });
        }
        if inv.kind == InvariantKind::Eventual && inv.window_ms.is_none() {
            return Err(CrossInvariantParseError::MissingEventualWindow {
                name: inv.name.clone(),
            });
        }
        if inv.kind == InvariantKind::Eventual && inv.window_ms == Some(0) {
            return Err(CrossInvariantParseError::InvalidEventualWindow {
                name: inv.name.clone(),
            });
        }
    }

    Ok(spec)
}

/// Parse the extended assertion grammar used by cross-invariant enforcement.
///
/// Accepts both the pre-ADR-0041 form:
///
/// ```text
/// related(TargetEntity, sourceField).status in ["A","B"]
/// ```
///
/// and the extended form:
///
/// ```text
/// related(TargetEntity, sourceField).<FieldName> in     ["A","B"]
/// related(TargetEntity, sourceField).<FieldName> not in ["A"]
/// ```
///
/// Returns `None` on any syntactic failure (caller converts to a validation
/// error). The `field_name` defaults to `"status"` when the tail omits it;
/// the old dot-status-in shape therefore parses as an equivalent
/// `RelatedFieldAssert` with `field_name = "status"` and `operator = In`.
pub fn parse_related_field_assert(input: &str) -> Option<RelatedFieldAssert> {
    let s = input.trim();
    let rest = s.strip_prefix("related(")?;
    let close_idx = rest.find(')')?;
    let args = rest[..close_idx].trim();
    let tail = rest[close_idx + 1..].trim();

    // Parse `(TargetEntity, sourceField)`.
    let mut arg_parts = args.split(',').map(str::trim);
    let target_entity = arg_parts.next()?.to_string();
    let source_field = arg_parts.next()?.to_string();
    if target_entity.is_empty() || source_field.is_empty() || arg_parts.next().is_some() {
        return None;
    }

    // Parse `.<FieldName> (not in|in) [ ... ]`.
    let tail = tail.strip_prefix('.')?;
    let open_bracket = tail.find('[')?;
    if !tail.ends_with(']') {
        return None;
    }
    let head = tail[..open_bracket].trim_end();
    let list_inner = tail[open_bracket + 1..tail.len() - 1].trim();

    // Split the head into `<FieldName> <operator>`. Operator is either
    // `in` or `not in` (two tokens), so look at the trailing whitespace-
    // separated tokens.
    let head_tokens: Vec<&str> = head.split_whitespace().collect();
    let (field_name, operator) = match head_tokens.as_slice() {
        [field, "in"] => (field.to_string(), CrossInvariantOperator::In),
        [field, "not", "in"] => (field.to_string(), CrossInvariantOperator::NotIn),
        _ => return None,
    };
    if !is_identifier(&field_name) {
        return None;
    }

    if list_inner.is_empty() {
        return None;
    }

    let mut values = Vec::new();
    for raw in list_inner.split(',') {
        let token = raw.trim();
        // Accept either double- or single-quoted string literals, consistent
        // with the legacy parser's leniency.
        let stripped = token
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .or_else(|| token.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))?;
        if stripped.is_empty() {
            return None;
        }
        values.push(stripped.to_string());
    }
    if values.is_empty() {
        return None;
    }

    Some(RelatedFieldAssert {
        target_entity,
        source_field,
        field_name,
        operator,
        values,
    })
}

/// Legacy wrapper returning only `in`-on-`status` assertions.
///
/// Retained so the public API doesn't shift — new callers should use
/// [`parse_related_field_assert`] directly.
pub fn parse_related_status_in_assert(input: &str) -> Option<RelatedStatusInAssert> {
    let assert = parse_related_field_assert(input)?;
    if !assert.is_legacy_status_in() {
        return None;
    }
    Some(RelatedStatusInAssert {
        target_entity: assert.target_entity,
        source_field: assert.source_field,
        statuses: assert.values,
    })
}

fn is_identifier(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let mut chars = s.chars();
    let first = chars.next().unwrap(); // ci-ok: non-empty checked above
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Split `Entity.*` or `Entity.Action` into `(entity, action)`.
///
/// `action` is `None` for the wildcard form. Returns `None` when the
/// string is not that shape.
pub fn split_trigger(trigger: &str) -> Option<(&str, Option<&str>)> {
    let (entity, action) = trigger.split_once('.')?;
    let entity = entity.trim();
    let action = action.trim();
    if entity.is_empty() || action.is_empty() {
        return None;
    }
    if action == "*" {
        return Some((entity, None));
    }
    Some((entity, Some(action)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cross_invariant::{DeletePolicy, InvariantKind};

    #[test]
    fn parse_minimal_doc() {
        let src = r#"
version = 1
default_delete_policy = "restrict"

[[invariant]]
name = "ship_requires_capture"
kind = "hard"
on = "Order.ShipOrder"
assert = "related(Payment, payment_id).status in [\"Captured\"]"
"#;
        let spec = parse_cross_invariants(src).expect("should parse");
        assert_eq!(spec.version, 1);
        assert_eq!(spec.default_delete_policy, DeletePolicy::Restrict);
        assert_eq!(spec.invariants.len(), 1);
        assert_eq!(spec.invariants[0].kind, InvariantKind::Hard);
    }

    #[test]
    fn parse_eventual_requires_window() {
        let src = r#"
[[invariant]]
name = "eventual_capture"
kind = "eventual"
on = "Order.ConfirmOrder"
assert = "related(Payment, payment_id).status in [\"Captured\"]"
"#;
        let err = parse_cross_invariants(src).expect_err("must fail");
        assert!(matches!(
            err,
            CrossInvariantParseError::MissingEventualWindow { .. }
        ));
    }

    #[test]
    fn parse_assert_syntax() {
        let parsed = parse_related_status_in_assert(
            r#"related(Payment, payment_id).status in ["Captured","Authorized"]"#,
        )
        .expect("should parse");
        assert_eq!(parsed.target_entity, "Payment");
        assert_eq!(parsed.source_field, "payment_id");
        assert_eq!(parsed.statuses, vec!["Captured", "Authorized"]);
    }

    #[test]
    fn parse_field_assert_legacy_status_in_form() {
        let parsed =
            parse_related_field_assert(r#"related(Payment, payment_id).status in ["Captured"]"#)
                .expect("should parse");
        assert_eq!(parsed.target_entity, "Payment");
        assert_eq!(parsed.source_field, "payment_id");
        assert_eq!(parsed.field_name, "status");
        assert_eq!(parsed.operator, CrossInvariantOperator::In);
        assert_eq!(parsed.values, vec!["Captured"]);
        assert!(parsed.is_legacy_status_in());
    }

    #[test]
    fn parse_field_assert_arbitrary_field_name() {
        let parsed = parse_related_field_assert(
            r#"related(Environment, EnvironmentId).ConfigType in ["Cloud"]"#,
        )
        .expect("should parse");
        assert_eq!(parsed.field_name, "ConfigType");
        assert_eq!(parsed.operator, CrossInvariantOperator::In);
        assert_eq!(parsed.values, vec!["Cloud"]);
        assert!(!parsed.is_legacy_status_in());
    }

    #[test]
    fn parse_field_assert_not_in_form() {
        let parsed = parse_related_field_assert(
            r#"related(Environment, EnvironmentId).ConfigType not in ["Local"]"#,
        )
        .expect("should parse");
        assert_eq!(parsed.field_name, "ConfigType");
        assert_eq!(parsed.operator, CrossInvariantOperator::NotIn);
        assert_eq!(parsed.values, vec!["Local"]);
    }

    #[test]
    fn parse_field_assert_rejects_bad_field_name() {
        assert!(
            parse_related_field_assert(r#"related(Environment, EnvironmentId).1Config in ["A"]"#)
                .is_none()
        );
    }

    #[test]
    fn parse_field_assert_rejects_empty_values() {
        assert!(
            parse_related_field_assert(r#"related(Environment, EnvironmentId).ConfigType in []"#)
                .is_none()
        );
    }

    #[test]
    fn parse_field_assert_rejects_unknown_operator() {
        assert!(
            parse_related_field_assert(
                r#"related(Environment, EnvironmentId).ConfigType equals ["A"]"#
            )
            .is_none()
        );
    }

    #[test]
    fn legacy_wrapper_rejects_non_status_field() {
        assert!(
            parse_related_status_in_assert(
                r#"related(Environment, EnvironmentId).ConfigType in ["Cloud"]"#
            )
            .is_none()
        );
    }

    #[test]
    fn legacy_wrapper_rejects_not_in() {
        assert!(
            parse_related_status_in_assert(
                r#"related(Payment, payment_id).status not in ["Refunded"]"#
            )
            .is_none()
        );
    }

    #[test]
    fn parse_doc_accepts_not_in_assertion() {
        let src = r#"
[[invariant]]
name = "allowed_host_requires_non_local_parent"
kind = "hard"
on = "EnvironmentAllowedHost.*"
assert = "related(Environment, EnvironmentId).ConfigType not in [\"Local\"]"
"#;
        let spec = parse_cross_invariants(src).expect("should parse");
        assert_eq!(spec.invariants.len(), 1);
    }

    #[test]
    fn parse_rejects_non_restrict_default_delete_policy() {
        let src = r#"
default_delete_policy = "cascade"

[[invariant]]
name = "ship_requires_capture"
on = "Order.ShipOrder"
assert = "related(Payment, payment_id).status in [\"Captured\"]"
"#;
        let err = parse_cross_invariants(src).expect_err("must fail");
        assert!(matches!(
            err,
            CrossInvariantParseError::UnsupportedDeletePolicy { .. }
        ));
    }

    #[test]
    fn parse_rejects_non_restrict_relation_override_policy() {
        let src = r#"
[[relation_override]]
from_entity = "Order"
navigation_property = "Payment"
delete_policy = "setnull"

[[invariant]]
name = "ship_requires_capture"
on = "Order.ShipOrder"
assert = "related(Payment, payment_id).status in [\"Captured\"]"
"#;
        let err = parse_cross_invariants(src).expect_err("must fail");
        assert!(matches!(
            err,
            CrossInvariantParseError::UnsupportedDeletePolicy { .. }
        ));
    }

    #[test]
    fn split_trigger_supports_wildcard() {
        assert_eq!(split_trigger("Order.*"), Some(("Order", None)));
        assert_eq!(
            split_trigger("Order.ConfirmOrder"),
            Some(("Order", Some("ConfirmOrder")))
        );
        assert_eq!(split_trigger("broken"), None);
    }
}
