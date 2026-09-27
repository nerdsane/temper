//! Cache mechanics and authorization isolation are both observable contracts.

use std::collections::HashMap;
use std::panic::catch_unwind;

use super::*;
use crate::context::SecurityContext;
use crate::engine::{AuthzDecision, AuthzEngine};
use crate::error::AuthzDenial;

const READ_PERMIT: &str =
    r#"permit(principal is Agent, action == Action::"read", resource is Doc);"#;
const READ_FORBID: &str =
    r#"forbid(principal is Agent, action == Action::"read", resource is Doc);"#;

fn local_cache(entries: usize, bytes: usize) -> Mutex<CompiledPolicyCache> {
    Mutex::new(CompiledPolicyCache::new(entries, bytes))
}

fn tenant_snapshot(engine: &AuthzEngine, tenant: &str) -> Arc<CompiledPolicies> {
    Arc::clone(&engine.tenant_policies.read().unwrap()[tenant].policies)
}

#[test]
fn cache_hits_reuse_the_complete_immutable_compilation() {
    let cache = local_cache(2, 1024);
    let first = compile_with_cache(READ_PERMIT, &cache).unwrap();
    let second = compile_with_cache(READ_PERMIT, &cache).unwrap();
    assert!(
        Arc::ptr_eq(&first, &second),
        "a hit must not compile or clone"
    );

    let mut fresh: PolicySet = READ_PERMIT.parse().unwrap();
    merge_system_platform_policy(&mut fresh);
    assert_eq!(first.policy_set, fresh);
    assert_eq!(cache.lock().unwrap().entries.len(), 1);
    assert_eq!(cache.lock().unwrap().source_bytes, READ_PERMIT.len());
}

#[test]
fn only_exact_source_matches_are_reused() {
    let cache = local_cache(3, 1024);
    let first = compile_with_cache(READ_PERMIT, &cache).unwrap();
    let different_format = format!("// same meaning, different source\n{READ_PERMIT}");
    let formatted = compile_with_cache(&different_format, &cache).unwrap();
    let changed = compile_with_cache(READ_FORBID, &cache).unwrap();
    assert!(!Arc::ptr_eq(&first, &formatted));
    assert!(!Arc::ptr_eq(&first, &changed));
    assert_eq!(first.policy_set, formatted.policy_set);
    assert_ne!(first.policy_set, changed.policy_set);
}

#[test]
fn fifo_eviction_obeys_entry_budget_without_refreshing_hits() {
    let cache = local_cache(2, 1024);
    let sources: Vec<String> = ['a', 'b', 'c']
        .into_iter()
        .map(|label| format!("// {label}\n{READ_PERMIT}"))
        .collect();
    let active = compile_with_cache(&sources[0], &cache).unwrap();
    compile_with_cache(&sources[1], &cache).unwrap();
    assert!(Arc::ptr_eq(
        &active,
        &compile_with_cache(&sources[0], &cache).unwrap(),
    ));
    compile_with_cache(&sources[2], &cache).unwrap();

    let cache = cache.lock().unwrap();
    assert!(cache.get(&sources[0]).is_none());
    assert!(cache.get(&sources[1]).is_some());
    assert!(cache.get(&sources[2]).is_some());
    assert_eq!(cache.entries.len(), 2);
    assert_eq!(cache.source_bytes, sources[1].len() + sources[2].len());
    let mut fresh: PolicySet = sources[0].parse().unwrap();
    merge_system_platform_policy(&mut fresh);
    assert_eq!(
        active.policy_set, fresh,
        "eviction leaves active snapshots intact"
    );
}

#[test]
fn source_byte_budget_evicts_before_the_entry_budget() {
    let cache = local_cache(8, READ_PERMIT.len() + READ_FORBID.len() - 1);
    compile_with_cache(READ_PERMIT, &cache).unwrap();
    compile_with_cache(READ_FORBID, &cache).unwrap();
    let cache = cache.lock().unwrap();
    assert!(cache.get(READ_PERMIT).is_none());
    assert!(cache.get(READ_FORBID).is_some());
    assert_eq!(cache.source_bytes, READ_FORBID.len());
    assert!(cache.source_bytes <= cache.source_byte_budget);
}

#[test]
fn oversized_and_disabled_caches_preserve_compilation_without_retention() {
    let oversized = format!("{READ_PERMIT}\n//{}", "x".repeat(MAX_CACHED_SOURCE_BYTES));
    for (source, cache) in [
        (oversized.as_str(), local_cache(2, TOTAL_SOURCE_BYTE_BUDGET)),
        (READ_PERMIT, local_cache(0, 1024)),
        (READ_PERMIT, local_cache(2, READ_PERMIT.len() - 1)),
    ] {
        let first = compile_with_cache(source, &cache).unwrap();
        let second = compile_with_cache(source, &cache).unwrap();
        assert!(!Arc::ptr_eq(&first, &second));
        assert_eq!(first.policy_set, second.policy_set);
        let cache = cache.lock().unwrap();
        assert!(cache.entries.is_empty());
        assert!(cache.insertion_order.is_empty());
        assert_eq!(cache.source_bytes, 0);
    }
}

#[test]
fn parse_errors_do_not_enter_or_evict_the_cache() {
    let cache = local_cache(1, 1024);
    let active = compile_with_cache(READ_PERMIT, &cache).unwrap();
    for _ in 0..2 {
        assert!(matches!(
            compile_with_cache("not cedar", &cache),
            Err(AuthzError::PolicyParse(_)),
        ));
    }
    let cache = cache.lock().unwrap();
    assert_eq!(cache.entries.len(), 1);
    assert!(Arc::ptr_eq(&active, &cache.get(READ_PERMIT).unwrap()));
}

#[test]
fn poisoned_cache_is_bypassed_without_changing_parse_results() {
    let cache = local_cache(1, 1024);
    assert!(
        catch_unwind(|| {
            let _guard = cache.lock().unwrap();
            panic!("deliberately poison this test-local cache");
        })
        .is_err()
    );
    assert!(cache.is_poisoned());

    let compiled = compile_with_cache(READ_PERMIT, &cache).unwrap();
    let mut fresh: PolicySet = READ_PERMIT.parse().unwrap();
    merge_system_platform_policy(&mut fresh);
    assert_eq!(compiled.policy_set, fresh);
    assert!(matches!(
        compile_with_cache("not cedar", &cache),
        Err(AuthzError::PolicyParse(_)),
    ));
}

#[test]
fn shared_raw_snapshots_do_not_share_tenant_activation() {
    let first = AuthzEngine::empty();
    let second = AuthzEngine::empty();
    // Install the same immutable snapshot explicitly, avoiding assumptions about
    // concurrent tests evicting the process-wide cache between engine calls.
    let cache = local_cache(2, 1024);
    let snapshot = compile_with_cache(READ_PERMIT, &cache).unwrap();
    for (engine, tenant) in [(&first, "a"), (&first, "b"), (&second, "a")] {
        engine.tenant_policies.write().unwrap().insert(
            tenant.to_string(),
            super::super::TenantPolicies {
                policies: Arc::clone(&snapshot),
                source_text: READ_PERMIT.to_string(),
            },
        );
    }
    assert!(Arc::ptr_eq(&tenant_snapshot(&first, "a"), &snapshot));
    assert!(Arc::ptr_eq(&tenant_snapshot(&second, "a"), &snapshot));

    let agent = SecurityContext::from_resolved_identity("reader", "worker", None);
    let attrs = HashMap::new();
    first.reload_tenant_policies("a", READ_FORBID).unwrap();
    assert!(matches!(
        first.authorize_for_tenant("a", &agent, "read", "Doc", &attrs),
        AuthzDecision::Deny(AuthzDenial::PolicyDenied { .. }),
    ));
    for (engine, tenant) in [(&first, "b"), (&second, "a")] {
        assert!(
            engine
                .authorize_for_tenant(tenant, &agent, "read", "Doc", &attrs)
                .is_allowed()
        );
        assert!(Arc::ptr_eq(&tenant_snapshot(engine, tenant), &snapshot));
    }
    first.remove_tenant("b");
    assert_eq!(
        first.authorize_for_tenant("b", &agent, "read", "Doc", &attrs),
        AuthzDecision::Deny(AuthzDenial::NoMatchingPermit),
    );
    assert!(
        second
            .authorize_for_tenant("a", &agent, "read", "Doc", &attrs)
            .is_allowed()
    );
}

#[test]
fn public_raw_reload_uses_new_source_and_preserves_last_good_on_error() {
    let engine = AuthzEngine::empty();
    let agent = SecurityContext::from_resolved_identity("reader", "worker", None);
    let attrs = HashMap::new();
    engine.reload_tenant_policies("a", READ_PERMIT).unwrap();
    let original = tenant_snapshot(&engine, "a");
    assert!(engine.validate_tenant_policies(READ_FORBID).is_ok());
    assert!(Arc::ptr_eq(&original, &tenant_snapshot(&engine, "a")));
    assert!(engine.reload_tenant_policies("a", "not cedar").is_err());
    assert!(Arc::ptr_eq(&original, &tenant_snapshot(&engine, "a")));

    engine.reload_tenant_policies("a", READ_FORBID).unwrap();
    assert!(!Arc::ptr_eq(&original, &tenant_snapshot(&engine, "a")));
    assert!(matches!(
        engine.authorize_for_tenant("a", &agent, "read", "Doc", &attrs),
        AuthzDecision::Deny(AuthzDenial::PolicyDenied { .. }),
    ));
    engine.reload_tenant_policies("a", "").unwrap();
    assert_eq!(
        engine.authorize_for_tenant("a", &agent, "read", "Doc", &attrs),
        AuthzDecision::Deny(AuthzDenial::NoMatchingPermit),
    );
    assert!(
        engine
            .authorize_for_tenant("a", &SecurityContext::system(), "read", "Doc", &attrs)
            .is_allowed()
    );
    assert_eq!(engine.get_tenant_policy_text("a").as_deref(), Some(""));
}

#[test]
fn named_reload_never_inherits_raw_or_other_tenant_policy_ids() {
    let engine = AuthzEngine::empty();
    engine.reload_tenant_policies("raw", READ_FORBID).unwrap();
    let entries = vec![("block".to_string(), READ_FORBID.to_string())];
    let agent = SecurityContext::from_resolved_identity("reader", "worker", None);
    let attrs = HashMap::new();
    for tenant in ["a", "b"] {
        engine
            .reload_tenant_policies_named(tenant, &entries)
            .unwrap();
        assert_eq!(
            engine.authorize_for_tenant(tenant, &agent, "read", "Doc", &attrs),
            AuthzDecision::Deny(AuthzDenial::PolicyDenied {
                policy_ids: vec![format!("{tenant}:block")],
            }),
        );
    }
}

#[test]
fn empty_engines_reuse_builtin_compilation_without_sharing_fallback_activation() {
    let first = AuthzEngine::empty();
    let second = AuthzEngine::empty();
    assert!(Arc::ptr_eq(
        &first.platform_policy_set,
        &second.platform_policy_set
    ));
    assert!(Arc::ptr_eq(
        &first.fallback_policy_set.read().unwrap(),
        &second.fallback_policy_set.read().unwrap(),
    ));
    first.reload_policies(READ_PERMIT).unwrap();
    let agent = SecurityContext::from_resolved_identity("reader", "worker", None);
    let attrs = HashMap::new();
    assert!(first.authorize(&agent, "read", "Doc", &attrs).is_allowed());
    assert_eq!(
        second.authorize(&agent, "read", "Doc", &attrs),
        AuthzDecision::Deny(AuthzDenial::NoMatchingPermit),
    );
    assert!(Arc::ptr_eq(
        &first.platform_policy_set,
        &second.platform_policy_set
    ));
}
