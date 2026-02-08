//! Redis key naming conventions for Temper.
//!
//! All keys are prefixed with `temper:` to namespace within shared Redis instances.
//! Multi-tenant keys include the tenant after the subsystem prefix:
//! `temper:{subsystem}:{tenant}:{entity_type}:{entity_id}`.
//!
//! Legacy (single-tenant) key functions remain for backward compatibility.

/// Key prefix for all Temper Redis keys.
pub const PREFIX: &str = "temper";

/// Build a mailbox stream key for an actor (legacy, no tenant scope).
/// Format: `temper:mailbox:{actor_id}`
pub fn mailbox_key(actor_id: &str) -> String {
    format!("{PREFIX}:mailbox:{actor_id}")
}

/// Build a tenant-scoped mailbox stream key for an actor.
/// Format: `temper:mailbox:{tenant}:{actor_id}`
pub fn tenant_mailbox_key(tenant: &str, actor_id: &str) -> String {
    format!("{PREFIX}:mailbox:{tenant}:{actor_id}")
}

/// Build a placement cache key for an entity (legacy, no tenant scope).
/// Format: `temper:placement:{entity_type}:{entity_id}`
pub fn placement_key(entity_type: &str, entity_id: &str) -> String {
    format!("{PREFIX}:placement:{entity_type}:{entity_id}")
}

/// Build a tenant-scoped placement cache key for an entity.
/// Format: `temper:placement:{tenant}:{entity_type}:{entity_id}`
pub fn tenant_placement_key(tenant: &str, entity_type: &str, entity_id: &str) -> String {
    format!("{PREFIX}:placement:{tenant}:{entity_type}:{entity_id}")
}

/// Build a distributed lock key.
/// Format: `temper:lock:{resource}`
pub fn lock_key(resource: &str) -> String {
    format!("{PREFIX}:lock:{resource}")
}

/// Build a cache key for an OData Function response (legacy, no tenant scope).
/// Format: `temper:cache:fn:{function_name}:{entity_id}`
pub fn function_cache_key(function_name: &str, entity_id: &str) -> String {
    format!("{PREFIX}:cache:fn:{function_name}:{entity_id}")
}

/// Build a tenant-scoped cache key for an OData Function response.
/// Format: `temper:cache:fn:{tenant}:{function_name}:{entity_id}`
pub fn tenant_function_cache_key(tenant: &str, function_name: &str, entity_id: &str) -> String {
    format!("{PREFIX}:cache:fn:{tenant}:{function_name}:{entity_id}")
}

/// Build a cache key for entity state (legacy, no tenant scope).
/// Format: `temper:cache:entity:{entity_type}:{entity_id}`
pub fn entity_cache_key(entity_type: &str, entity_id: &str) -> String {
    format!("{PREFIX}:cache:entity:{entity_type}:{entity_id}")
}

/// Build a tenant-scoped cache key for entity state.
/// Format: `temper:cache:entity:{tenant}:{entity_type}:{entity_id}`
pub fn tenant_entity_cache_key(tenant: &str, entity_type: &str, entity_id: &str) -> String {
    format!("{PREFIX}:cache:entity:{tenant}:{entity_type}:{entity_id}")
}

/// Build the placement hash map key (stores all placements).
/// Format: `temper:placement`
pub fn placement_map_key() -> String {
    format!("{PREFIX}:placement")
}

/// Build a consumer group name for a mailbox stream.
/// Format: `temper:group:{actor_type}`
pub fn consumer_group(actor_type: &str) -> String {
    format!("{PREFIX}:group:{actor_type}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mailbox_key() {
        assert_eq!(mailbox_key("order-123"), "temper:mailbox:order-123");
    }

    #[test]
    fn test_placement_key() {
        assert_eq!(
            placement_key("Order", "abc-123"),
            "temper:placement:Order:abc-123"
        );
    }

    #[test]
    fn test_lock_key() {
        assert_eq!(lock_key("shard:7"), "temper:lock:shard:7");
    }

    #[test]
    fn test_function_cache_key() {
        assert_eq!(
            function_cache_key("GetOrderTotal", "order-456"),
            "temper:cache:fn:GetOrderTotal:order-456"
        );
    }

    #[test]
    fn test_entity_cache_key() {
        assert_eq!(
            entity_cache_key("Order", "abc"),
            "temper:cache:entity:Order:abc"
        );
    }

    #[test]
    fn test_all_keys_have_temper_prefix() {
        assert!(mailbox_key("x").starts_with("temper:"));
        assert!(placement_key("A", "B").starts_with("temper:"));
        assert!(lock_key("x").starts_with("temper:"));
        assert!(function_cache_key("F", "E").starts_with("temper:"));
        assert!(entity_cache_key("T", "I").starts_with("temper:"));
        assert!(placement_map_key().starts_with("temper:"));
        assert!(consumer_group("T").starts_with("temper:"));
    }

    // -- tenant-scoped keys ------------------------------------------------

    #[test]
    fn test_tenant_mailbox_key() {
        assert_eq!(
            tenant_mailbox_key("ecommerce", "order-123"),
            "temper:mailbox:ecommerce:order-123"
        );
    }

    #[test]
    fn test_tenant_placement_key() {
        assert_eq!(
            tenant_placement_key("linear", "Issue", "ISS-1"),
            "temper:placement:linear:Issue:ISS-1"
        );
    }

    #[test]
    fn test_tenant_entity_cache_key() {
        assert_eq!(
            tenant_entity_cache_key("ecommerce", "Order", "abc"),
            "temper:cache:entity:ecommerce:Order:abc"
        );
    }

    #[test]
    fn test_tenant_function_cache_key() {
        assert_eq!(
            tenant_function_cache_key("linear", "GetIssueCount", "sprint-1"),
            "temper:cache:fn:linear:GetIssueCount:sprint-1"
        );
    }

    #[test]
    fn test_tenant_keys_isolate_tenants() {
        let ecom = tenant_entity_cache_key("ecommerce", "Order", "1");
        let linear = tenant_entity_cache_key("linear", "Order", "1");
        assert_ne!(ecom, linear, "different tenants must produce different keys");
    }
}
