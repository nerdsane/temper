use thiserror::Error;

use super::types::{CrossInvariantSpec, DeletePolicy, InvariantKind};

/// Parsed assertion form:
/// `related(TargetEntity, sourceField).status in ["StateA","StateB"]`
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
        if parse_related_status_in_assert(&inv.assertion).is_none() {
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

/// Parse assertion syntax used by v1 cross-invariant enforcement:
/// `related(TargetEntity, sourceField).status in ["A","B"]`
pub fn parse_related_status_in_assert(input: &str) -> Option<RelatedStatusInAssert> {
    let s = input.trim();
    let rest = s.strip_prefix("related(")?;
    let close_idx = rest.find(')')?;
    let args = rest[..close_idx].trim();
    let tail = rest[close_idx + 1..].trim();

    if !tail.starts_with(".status in [") || !tail.ends_with(']') {
        return None;
    }

    let mut arg_parts = args.split(',').map(str::trim);
    let target_entity = arg_parts.next()?.to_string();
    let source_field = arg_parts.next()?.to_string();
    if target_entity.is_empty() || source_field.is_empty() || arg_parts.next().is_some() {
        return None;
    }

    let status_list = tail.strip_prefix(".status in [")?.strip_suffix(']')?.trim();
    if status_list.is_empty() {
        return None;
    }

    let mut statuses = Vec::new();
    for raw in status_list.split(',') {
        let token = raw.trim().trim_matches('"').trim_matches('\'').trim();
        if token.is_empty() {
            return None;
        }
        statuses.push(token.to_string());
    }
    if statuses.is_empty() {
        return None;
    }

    Some(RelatedStatusInAssert {
        target_entity,
        source_field,
        statuses,
    })
}

pub(crate) fn split_trigger(trigger: &str) -> Option<(&str, Option<&str>)> {
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
