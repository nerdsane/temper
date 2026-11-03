//! Which entity types a policy set's permits can reach, read from their scopes.

use std::collections::BTreeSet;
use std::str::FromStr;

use cedar_policy::{Effect, PolicySet, ResourceConstraint};

use crate::error::AuthzError;

/// The entity types, by name, that some permit in `policy_text` can reach.
/// `None` when a permit names no resource type in its scope (any resource, or
/// `in` a group), so it can reach every type.
pub fn permitted_resource_types(policy_text: &str) -> Result<Option<BTreeSet<String>>, AuthzError> {
    let policies =
        PolicySet::from_str(policy_text).map_err(|e| AuthzError::PolicyParse(e.to_string()))?;
    let mut types = BTreeSet::new();
    for policy in policies.policies().filter(|p| p.effect() == Effect::Permit) {
        let named = match policy.resource_constraint() {
            ResourceConstraint::Any | ResourceConstraint::In(_) => return Ok(None),
            ResourceConstraint::Is(name) | ResourceConstraint::IsIn(name, _) => name,
            ResourceConstraint::Eq(uid) => uid.type_name().clone(),
        };
        types.insert(named.basename().to_string());
    }
    Ok(Some(types))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_permit_reaches_the_types_its_scope_names() {
        let reach = permitted_resource_types(
            r#"
            permit(principal, action, resource is Session);
            permit(principal, action, resource == Temper::Task::"t1");
            forbid(principal, action, resource is Cron);
            "#,
        )
        .unwrap();
        assert_eq!(
            reach,
            Some(BTreeSet::from(["Session".to_string(), "Task".to_string()]))
        );
    }

    #[test]
    fn a_permit_on_any_resource_reaches_every_type() {
        let reach = permitted_resource_types(
            "permit(principal, action, resource is Session);\npermit(principal, action, resource);",
        )
        .unwrap();
        assert_eq!(reach, None);
    }
}
