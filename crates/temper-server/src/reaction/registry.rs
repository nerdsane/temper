//! Reaction rule registry with tenant isolation and TOML parsing.
//!
//! Uses `BTreeMap` throughout for deterministic iteration order (DST compliance).

use std::collections::BTreeMap;

use temper_runtime::tenant::TenantId;

use super::types::{
    MAX_REACTIONS_PER_TENANT, ReactionRule, ReactionTarget, ReactionTrigger, TargetResolver,
};

/// Registry of reaction rules, indexed per-tenant for fast lookup.
///
/// Rules are indexed by `"EntityType:Action"` for exact matches and
/// `"EntityType:*"` for wildcard (any-action) rules.
#[derive(Debug, Clone, Default)]
pub struct ReactionRegistry {
    /// Per-tenant rule index: key = "EntityType:Action" or "EntityType:*".
    tenants: BTreeMap<TenantId, BTreeMap<String, Vec<ReactionRule>>>,
}

impl ReactionRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register reaction rules for a tenant.
    ///
    /// # Panics
    ///
    /// Panics if the number of rules exceeds `MAX_REACTIONS_PER_TENANT`
    /// (TigerStyle: budget assertion, fail fast on resource exhaustion).
    pub fn register_tenant_rules(&mut self, tenant: impl Into<TenantId>, rules: Vec<ReactionRule>) {
        let tenant = tenant.into();
        assert!(
            rules.len() <= MAX_REACTIONS_PER_TENANT,
            "Tenant '{tenant}' has {} reaction rules, exceeding budget of {MAX_REACTIONS_PER_TENANT}",
            rules.len()
        );

        let mut index: BTreeMap<String, Vec<ReactionRule>> = BTreeMap::new();
        for rule in rules {
            let key = match &rule.when.action {
                Some(action) => format!("{}:{}", rule.when.entity_type, action),
                None => format!("{}:*", rule.when.entity_type),
            };
            index.entry(key).or_default().push(rule);
        }
        self.tenants.insert(tenant, index);
    }

    /// Look up matching reaction rules for a (tenant, entity_type, action, to_state) tuple.
    ///
    /// Returns rules matching both exact `"EntityType:Action"` and wildcard
    /// `"EntityType:*"` keys. If `to_state` is specified on a rule, only rules
    /// matching that state are returned.
    pub fn lookup(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        action: &str,
        to_state: &str,
    ) -> Vec<&ReactionRule> {
        let Some(index) = self.tenants.get(tenant) else {
            return Vec::new();
        };

        let exact_key = format!("{entity_type}:{action}");
        let wildcard_key = format!("{entity_type}:*");

        let mut results = Vec::new();

        // Exact match rules
        if let Some(rules) = index.get(&exact_key) {
            for rule in rules {
                if matches_state_filter(rule, to_state) {
                    results.push(rule);
                }
            }
        }

        // Wildcard rules (any action on this entity type)
        if let Some(rules) = index.get(&wildcard_key) {
            for rule in rules {
                if matches_state_filter(rule, to_state) {
                    results.push(rule);
                }
            }
        }

        results
    }

    /// Check if a tenant has any registered rules.
    pub fn has_rules(&self, tenant: &TenantId) -> bool {
        self.tenants.get(tenant).is_some_and(|idx| !idx.is_empty())
    }
}

/// Check if a rule's `to_state` filter matches the actual state.
fn matches_state_filter(rule: &ReactionRule, to_state: &str) -> bool {
    match &rule.when.to_state {
        Some(expected) => expected == to_state,
        None => true,
    }
}

/// Intermediate TOML structure for parsing a reactions file.
#[derive(serde::Deserialize)]
struct ReactionsFile {
    #[serde(default)]
    reaction: Vec<ReactionToml>,
}

/// TOML representation of a single reaction rule.
#[derive(serde::Deserialize)]
struct ReactionToml {
    name: String,
    when: TriggerToml,
    then: TargetToml,
    resolve_target: ResolverToml,
}

#[derive(serde::Deserialize)]
struct TriggerToml {
    entity_type: String,
    action: Option<String>,
    to_state: Option<String>,
}

#[derive(serde::Deserialize)]
struct TargetToml {
    entity_type: String,
    action: String,
    #[serde(default)]
    params: Option<serde_json::Value>,
}

#[derive(serde::Deserialize)]
struct ResolverToml {
    #[serde(rename = "type")]
    resolver_type: String,
    field: Option<String>,
    entity_id: Option<String>,
    id_field: Option<String>,
}

/// Parse reaction rules from a TOML string.
///
/// Expects the format:
/// ```toml
/// [[reaction]]
/// name = "rule_name"
/// [reaction.when]
/// entity_type = "Order"
/// action = "ConfirmOrder"
/// to_state = "Confirmed"
/// [reaction.then]
/// entity_type = "Payment"
/// action = "AuthorizePayment"
/// [reaction.resolve_target]
/// type = "field"
/// field = "payment_id"
/// ```
pub fn parse_reactions(toml_str: &str) -> Result<Vec<ReactionRule>, String> {
    let file: ReactionsFile =
        toml::from_str(toml_str).map_err(|e| format!("Failed to parse reactions TOML: {e}"))?;

    let mut rules = Vec::new();
    for r in file.reaction {
        let resolve_target = match r.resolve_target.resolver_type.as_str() {
            "field" => {
                let field = r.resolve_target.field.ok_or_else(|| {
                    format!(
                        "Reaction '{}': 'field' resolver requires 'field' key",
                        r.name
                    )
                })?;
                TargetResolver::Field { field }
            }
            "same_id" => TargetResolver::SameId,
            "static" => {
                let entity_id = r.resolve_target.entity_id.ok_or_else(|| {
                    format!(
                        "Reaction '{}': 'static' resolver requires 'entity_id' key",
                        r.name
                    )
                })?;
                TargetResolver::Static { entity_id }
            }
            "create_if_missing" => {
                let id_field = r.resolve_target.id_field.ok_or_else(|| {
                    format!(
                        "Reaction '{}': 'create_if_missing' resolver requires 'id_field' key",
                        r.name
                    )
                })?;
                TargetResolver::CreateIfMissing { id_field }
            }
            other => {
                return Err(format!(
                    "Reaction '{}': unknown resolver type '{other}'",
                    r.name
                ));
            }
        };

        rules.push(ReactionRule {
            name: r.name,
            when: ReactionTrigger {
                entity_type: r.when.entity_type,
                action: r.when.action,
                to_state: r.when.to_state,
            },
            then: ReactionTarget {
                entity_type: r.then.entity_type,
                action: r.then.action,
                params: r.then.params.unwrap_or(serde_json::json!({})),
            },
            resolve_target,
        });
    }

    Ok(rules)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_rule(
        name: &str,
        entity_type: &str,
        action: Option<&str>,
        to_state: Option<&str>,
        target_type: &str,
        target_action: &str,
    ) -> ReactionRule {
        ReactionRule {
            name: name.to_string(),
            when: ReactionTrigger {
                entity_type: entity_type.to_string(),
                action: action.map(|s| s.to_string()),
                to_state: to_state.map(|s| s.to_string()),
            },
            then: ReactionTarget {
                entity_type: target_type.to_string(),
                action: target_action.to_string(),
                params: serde_json::json!({}),
            },
            resolve_target: TargetResolver::SameId,
        }
    }

    #[test]
    fn lookup_exact_match() {
        let mut reg = ReactionRegistry::new();
        reg.register_tenant_rules(
            "t1",
            vec![sample_rule(
                "r1",
                "Order",
                Some("ConfirmOrder"),
                Some("Confirmed"),
                "Payment",
                "Authorize",
            )],
        );

        let tenant = TenantId::new("t1");
        let results = reg.lookup(&tenant, "Order", "ConfirmOrder", "Confirmed");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "r1");
    }

    #[test]
    fn lookup_wildcard_match() {
        let mut reg = ReactionRegistry::new();
        reg.register_tenant_rules(
            "t1",
            vec![sample_rule(
                "audit", "Order", None, None, "AuditLog", "Record",
            )],
        );

        let tenant = TenantId::new("t1");
        let results = reg.lookup(&tenant, "Order", "AnyAction", "AnyState");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "audit");
    }

    #[test]
    fn lookup_state_filter_excludes_non_matching() {
        let mut reg = ReactionRegistry::new();
        reg.register_tenant_rules(
            "t1",
            vec![sample_rule(
                "r1",
                "Order",
                Some("ConfirmOrder"),
                Some("Confirmed"),
                "Payment",
                "Authorize",
            )],
        );

        let tenant = TenantId::new("t1");
        // Wrong to_state
        let results = reg.lookup(&tenant, "Order", "ConfirmOrder", "Cancelled");
        assert!(results.is_empty());
    }

    #[test]
    fn lookup_no_match_wrong_entity() {
        let mut reg = ReactionRegistry::new();
        reg.register_tenant_rules(
            "t1",
            vec![sample_rule(
                "r1",
                "Order",
                Some("ConfirmOrder"),
                None,
                "Payment",
                "Authorize",
            )],
        );

        let tenant = TenantId::new("t1");
        let results = reg.lookup(&tenant, "Payment", "ConfirmOrder", "Confirmed");
        assert!(results.is_empty());
    }

    #[test]
    fn lookup_wrong_tenant_returns_empty() {
        let mut reg = ReactionRegistry::new();
        reg.register_tenant_rules(
            "t1",
            vec![sample_rule(
                "r1",
                "Order",
                Some("ConfirmOrder"),
                None,
                "Payment",
                "Authorize",
            )],
        );

        let tenant = TenantId::new("t2");
        let results = reg.lookup(&tenant, "Order", "ConfirmOrder", "Confirmed");
        assert!(results.is_empty());
    }

    #[test]
    fn lookup_combines_exact_and_wildcard() {
        let mut reg = ReactionRegistry::new();
        reg.register_tenant_rules(
            "t1",
            vec![
                sample_rule(
                    "exact",
                    "Order",
                    Some("ConfirmOrder"),
                    None,
                    "Payment",
                    "Authorize",
                ),
                sample_rule("wildcard", "Order", None, None, "AuditLog", "Record"),
            ],
        );

        let tenant = TenantId::new("t1");
        let results = reg.lookup(&tenant, "Order", "ConfirmOrder", "Confirmed");
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn parse_reactions_valid_toml() {
        let toml = r#"
[[reaction]]
name = "order_confirmed_triggers_payment"
[reaction.when]
entity_type = "Order"
action = "ConfirmOrder"
to_state = "Confirmed"
[reaction.then]
entity_type = "Payment"
action = "AuthorizePayment"
[reaction.resolve_target]
type = "field"
field = "payment_id"

[[reaction]]
name = "audit_all_orders"
[reaction.when]
entity_type = "Order"
[reaction.then]
entity_type = "AuditLog"
action = "Record"
[reaction.resolve_target]
type = "same_id"
"#;
        let rules = parse_reactions(toml).unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].name, "order_confirmed_triggers_payment");
        assert!(
            matches!(rules[0].resolve_target, TargetResolver::Field { ref field } if field == "payment_id")
        );
        assert_eq!(rules[1].name, "audit_all_orders");
        assert!(matches!(rules[1].resolve_target, TargetResolver::SameId));
    }

    #[test]
    fn parse_reactions_empty_file() {
        let rules = parse_reactions("").unwrap();
        assert!(rules.is_empty());
    }

    #[test]
    fn parse_reactions_missing_resolver_field() {
        let toml = r#"
[[reaction]]
name = "bad"
[reaction.when]
entity_type = "Order"
[reaction.then]
entity_type = "Payment"
action = "Pay"
[reaction.resolve_target]
type = "field"
"#;
        let result = parse_reactions(toml);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("'field' resolver requires 'field' key")
        );
    }

    #[test]
    fn parse_reactions_unknown_resolver_type() {
        let toml = r#"
[[reaction]]
name = "bad"
[reaction.when]
entity_type = "Order"
[reaction.then]
entity_type = "Payment"
action = "Pay"
[reaction.resolve_target]
type = "magic"
"#;
        let result = parse_reactions(toml);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("unknown resolver type 'magic'")
        );
    }

    #[test]
    fn parse_reactions_all_resolver_types() {
        let toml = r#"
[[reaction]]
name = "field_resolver"
[reaction.when]
entity_type = "A"
[reaction.then]
entity_type = "B"
action = "Do"
[reaction.resolve_target]
type = "field"
field = "b_id"

[[reaction]]
name = "same_id_resolver"
[reaction.when]
entity_type = "A"
[reaction.then]
entity_type = "B"
action = "Do"
[reaction.resolve_target]
type = "same_id"

[[reaction]]
name = "static_resolver"
[reaction.when]
entity_type = "A"
[reaction.then]
entity_type = "B"
action = "Do"
[reaction.resolve_target]
type = "static"
entity_id = "singleton-1"

[[reaction]]
name = "create_resolver"
[reaction.when]
entity_type = "A"
[reaction.then]
entity_type = "B"
action = "Do"
[reaction.resolve_target]
type = "create_if_missing"
id_field = "b_id"
"#;
        let rules = parse_reactions(toml).unwrap();
        assert_eq!(rules.len(), 4);
        assert!(
            matches!(rules[0].resolve_target, TargetResolver::Field { ref field } if field == "b_id")
        );
        assert!(matches!(rules[1].resolve_target, TargetResolver::SameId));
        assert!(
            matches!(rules[2].resolve_target, TargetResolver::Static { ref entity_id } if entity_id == "singleton-1")
        );
        assert!(
            matches!(rules[3].resolve_target, TargetResolver::CreateIfMissing { ref id_field } if id_field == "b_id")
        );
    }

    #[test]
    #[should_panic(expected = "exceeding budget")]
    fn budget_assertion_on_too_many_rules() {
        let mut reg = ReactionRegistry::new();
        let rules: Vec<ReactionRule> = (0..MAX_REACTIONS_PER_TENANT + 1)
            .map(|i| sample_rule(&format!("r{i}"), "Order", None, None, "Target", "Do"))
            .collect();
        reg.register_tenant_rules("t1", rules);
    }
}
