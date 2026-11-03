use std::collections::{BTreeSet, HashMap};

use super::*;
use crate::context::{Principal, SecurityContext};
use crate::engine::AuthzEngine;

fn customer() -> SecurityContext {
    SecurityContext {
        principal: Principal {
            id: "reader".to_string(),
            kind: PrincipalKind::Customer,
            role: None,
            acting_for: None,
            agent_type: None,
            label: None,
            attributes: HashMap::new(),
        },
        context_attrs: HashMap::new(),
        correlation_id: "list-uniformity".to_string(),
        cause: None,
        credential: None,
    }
}

fn classify_for(policy: &str, resource_type: &str) -> ListReadUniformity {
    classify_as(&customer(), policy, resource_type)
}

fn classify_as(reader: &SecurityContext, policy: &str, resource_type: &str) -> ListReadUniformity {
    let engine = AuthzEngine::empty();
    engine
        .reload_tenant_policies("tenant", policy)
        .expect("policy should parse");
    engine.list_read_uniformity("tenant", reader, "read", resource_type)
}

fn per_entity_keys(decision: ListReadUniformity) -> BTreeSet<String> {
    match decision {
        ListReadUniformity::PerEntity {
            attribute_keys: Some(keys),
        } => keys,
        other => panic!("expected per-entity with listed keys, got {other:?}"),
    }
}

/// A row's `owner` field reaches Cedar as `context.owner`, so a forbid on it
/// tells rows apart: decided once without fields, it would not apply and
/// the reader would see every owner's rows.
#[test]
fn a_forbid_on_a_context_key_a_row_fills_is_per_entity() {
    let decision = classify_for(
        r#"
        permit(principal, action == Action::"read", resource is Order);
        forbid(principal, action == Action::"read", resource is Order)
        when { context has owner && context.owner != principal.id };
        "#,
        "Order",
    );
    assert!(per_entity_keys(decision).contains("owner"));
}

#[test]
fn a_permit_on_a_context_key_a_row_fills_is_per_entity() {
    let decision = classify_for(
        r#"
        permit(principal, action == Action::"read", resource is Order)
        when { context.owner == principal.id };
        "#,
        "Order",
    );
    assert!(per_entity_keys(decision).contains("owner"));
}

/// Keys the request fills are the same for every row: authority keys, and
/// the request's own context attributes, which a row's field cannot replace.
#[test]
fn context_keys_the_request_fills_stay_uniform() {
    let mut reader = customer();
    reader
        .context_attrs
        .insert("module".to_string(), serde_json::json!("steps"));
    let decision = classify_as(
        &reader,
        r#"
        permit(principal, action == Action::"read", resource is Order)
        when { context has module && context.module == "steps" && context has agentType };
        "#,
        "Order",
    );
    assert_eq!(decision, ListReadUniformity::Uniform);
}

#[test]
fn a_nested_read_of_a_context_key_a_row_fills_reads_the_whole_row() {
    let decision = classify_for(
        r#"
        permit(principal, action == Action::"read", resource is Order)
        when { context.owner.name == "ada" };
        "#,
        "Order",
    );
    assert_eq!(
        decision,
        ListReadUniformity::PerEntity {
            attribute_keys: None
        }
    );
}

#[test]
fn type_level_read_permit_is_uniform() {
    let decision = classify_for(
        r#"permit(principal, action == Action::"read", resource is DesignLanguage);"#,
        "DesignLanguage",
    );
    assert_eq!(decision, ListReadUniformity::Uniform);
}

#[test]
fn has_spec_condition_stays_uniform() {
    let decision = classify_for(
        r#"
        permit(principal, action == Action::"read", resource is Order)
        when { resource.has_spec == true };
        "#,
        "Order",
    );
    assert_eq!(decision, ListReadUniformity::Uniform);
}

#[test]
fn status_condition_is_per_entity() {
    let decision = classify_for(
        r#"
        permit(principal, action == Action::"read", resource is DesignLanguage)
        when { resource.status == "published" };
        "#,
        "DesignLanguage",
    );
    match decision {
        ListReadUniformity::PerEntity {
            attribute_keys: Some(keys),
        } => {
            assert!(keys.contains("status"), "keys={keys:?}");
            assert!(!keys.contains("has_spec"));
        }
        other => panic!("expected per-entity status, got {other:?}"),
    }
}

#[test]
fn resource_id_condition_is_per_entity_without_json_fields() {
    let decision = classify_for(
        r#"
        permit(principal, action == Action::"read", resource is Order)
        when { resource.id == "ord-01" };
        "#,
        "Order",
    );
    match decision {
        ListReadUniformity::PerEntity {
            attribute_keys: Some(keys),
        } => assert!(
            keys.is_empty(),
            "resource.id is the row id, not a catalog JSON field: {keys:?}"
        ),
        other => panic!("expected per-entity id condition, got {other:?}"),
    }
}

#[test]
fn nested_resource_attribute_is_not_projected() {
    let decision = classify_for(
        r#"
        permit(principal, action == Action::"read", resource is Order)
        when { resource.owner.name == "ada" };
        "#,
        "Order",
    );
    assert_eq!(
        decision,
        ListReadUniformity::PerEntity {
            attribute_keys: None
        }
    );
}

#[test]
fn entity_id_forbid_is_per_entity_without_loading_json_fields() {
    let decision = classify_for(
        r#"
        permit(principal, action == Action::"read", resource is Order);
        forbid(principal, action == Action::"read", resource == Order::"ord-01");
        "#,
        "Order",
    );
    match decision {
        ListReadUniformity::PerEntity {
            attribute_keys: Some(keys),
        } => assert!(
            keys.is_empty(),
            "an id forbid needs the entity id, not catalog JSON: {keys:?}"
        ),
        other => panic!("expected per-entity id forbid, got {other:?}"),
    }
}

#[test]
fn unrelated_type_and_action_do_not_force_per_row_reads() {
    let decision = classify_for(
        r#"
        permit(principal, action == Action::"read", resource is Order);
        forbid(principal, action == Action::"read", resource is TrustedIssuer);
        forbid(principal, action == Action::"delete", resource == Order::"ord-01");
        "#,
        "Order",
    );
    assert_eq!(decision, ListReadUniformity::Uniform);
}
