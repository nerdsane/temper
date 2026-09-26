//! Reaction rule registry with tenant isolation. Rules are synthesized from
//! entity-kind `[[action.triggers]]` (see `SpecRegistry::build_reaction_registry`).
//!
//! Uses `BTreeMap` throughout for deterministic iteration order (DST compliance).

use std::collections::BTreeMap;

use temper_runtime::tenant::TenantId;

use super::types::{MAX_REACTIONS_PER_TENANT, ReactionRule};

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
    /// Rule count is reported, not enforced. It used to be a TigerStyle budget
    /// assertion, which was the wrong instrument here: a budget assertion earns
    /// its place when exceeding it would corrupt something, as it does where
    /// memory is preallocated and a fixed limit is load-bearing. Nothing here
    /// is preallocated — rules live in growable maps, the constant sized no
    /// buffer and bounded no loop, and rule 257 costs exactly what rule 250
    /// costs. So the assertion protected nothing and cost everything: it was
    /// tenant-wide, so no single app could be written to respect it, and every
    /// app installed into a tenant tightened it for the rest. On 2026-09-10 a
    /// tenant hosting fifteen apps reached 265 rules; registration panicked, and
    /// because registration also happens at startup while replaying specs
    /// already committed to disk, the panic was on the main thread. The platform
    /// crash-looped and could only be recovered by deleting rows by hand. A
    /// limit that takes the system down to prevent a larger map is not a
    /// safeguard.
    ///
    /// [`MAX_REACTIONS_PER_TENANT`] survives as the threshold for a warning, so
    /// a tenant accumulating triggers without bound is still visible. The limit
    /// worth keeping is [`MAX_REACTION_DEPTH`](super::MAX_REACTION_DEPTH),
    /// which bounds recursive cascades — an unbounded reaction loop is a real
    /// failure mode, and depth is the thing that has no natural stopping point.
    pub fn register_tenant_rules(&mut self, tenant: impl Into<TenantId>, rules: Vec<ReactionRule>) {
        let tenant = tenant.into();
        if rules.len() > MAX_REACTIONS_PER_TENANT {
            tracing::warn!(
                tenant = %tenant,
                rules = rules.len(),
                advisory_threshold = MAX_REACTIONS_PER_TENANT,
                "tenant is carrying an unusually large number of reaction rules; registering them anyway"
            );
        }

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

    /// Number of rules registered for a tenant.
    pub fn rule_count(&self, tenant: &TenantId) -> usize {
        self.tenants
            .get(tenant)
            .map_or(0, |idx| idx.values().map(Vec::len).sum())
    }
}

/// Check if a rule's `to_state` filter matches the actual state.
fn matches_state_filter(rule: &ReactionRule, to_state: &str) -> bool {
    match &rule.when.to_state {
        Some(expected) => expected == to_state,
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trigger::types::{ReactionTarget, ReactionTrigger, TargetResolver};

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
                guard: None,
            },
            then: ReactionTarget {
                entity_type: target_type.to_string(),
                action: target_action.to_string(),
                params: serde_json::json!({}),
                params_from: BTreeMap::new(),
            },
            resolve_target: TargetResolver::SameId,
            principal: None,
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
    fn a_tenant_past_the_advisory_threshold_still_registers_every_rule() {
        let mut reg = ReactionRegistry::new();
        let over = MAX_REACTIONS_PER_TENANT + 9;
        let rules: Vec<ReactionRule> = (0..over)
            .map(|i| sample_rule(&format!("r{i}"), "Order", None, None, "Target", "Do"))
            .collect();

        reg.register_tenant_rules("t1", rules);

        // The rules are not merely accepted, they are findable: the dispatcher
        // asks exactly this question, and the ones past the old ceiling answer
        // it exactly like the ones below it.
        let found = reg.lookup(&TenantId::from("t1"), "Order", "Placed", "");
        assert_eq!(found.len(), over);
    }
}
