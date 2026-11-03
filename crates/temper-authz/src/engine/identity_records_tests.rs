//! Only the platform writes the records the bearer resolver trusts as a
//! verified identity (`AgentCredential`, `AgentType`), whatever a tenant permits.
use super::*;

const DIRECTED_EVOLUTION_POLICY: &str =
    include_str!("../../../../os-apps/directed-evolution/policies/directed_evolution.cedar");

/// Every write the two specs and the generic OData path offer.
const WRITES: &[(&str, &str)] = &[
    ("AgentCredential", "create"),
    ("AgentCredential", "update"),
    ("AgentCredential", "delete"),
    ("AgentCredential", "Issue"),
    ("AgentCredential", "Rotate"),
    ("AgentCredential", "Revoke"),
    ("AgentType", "create"),
    ("AgentType", "update"),
    ("AgentType", "delete"),
    ("AgentType", "Define"),
    ("AgentType", "Update"),
    ("AgentType", "Deprecate"),
    ("AgentType", "Reactivate"),
];

fn admin() -> SecurityContext {
    SecurityContext {
        principal: crate::context::Principal {
            id: "admin-1".to_string(),
            kind: PrincipalKind::Admin,
            role: None,
            acting_for: None,
            agent_type: None,
            label: None,
            attributes: HashMap::new(),
        },
        context_attrs: HashMap::new(),
        correlation_id: "test-admin".to_string(),
        cause: None,
        credential: None,
    }
}

fn agent(id: &str, agent_type: &str) -> SecurityContext {
    SecurityContext::from_resolved_identity(id, agent_type, None)
}

fn allowed(engine: &AuthzEngine, ctx: &SecurityContext, action: &str, resource: &str) -> bool {
    engine
        .authorize(ctx, action, resource, &HashMap::new())
        .is_allowed()
}

#[test]
fn an_app_agent_cannot_write_agent_credentials_or_types_under_a_broad_permit() {
    let directed_evolution =
        AuthzEngine::new(DIRECTED_EVOLUTION_POLICY).expect("directed-evolution policy parses");
    for engine in [AuthzEngine::permissive(), directed_evolution] {
        for (resource, action) in WRITES {
            for ctx in [agent("worker-1", "worker"), agent("human-1", "human")] {
                assert!(
                    !allowed(&engine, &ctx, action, resource),
                    "{} may not {action} {resource}",
                    ctx.principal.id
                );
            }
        }
    }
}

#[test]
fn the_platform_and_the_operator_still_write_agent_credentials_and_types() {
    let engine = AuthzEngine::permissive();
    for (resource, action) in WRITES {
        assert!(
            allowed(&engine, &SecurityContext::system(), action, resource),
            "System may {action} {resource}"
        );
        assert!(
            allowed(&engine, &admin(), action, resource),
            "Admin may {action} {resource}"
        );
        assert!(
            allowed(&engine, &agent("operator", "operator"), action, resource),
            "the verified operator may {action} {resource}"
        );
    }
    // A context that only claims the operator type is not the operator.
    let unverified =
        SecurityContext::anonymous().with_agent_context(Some("operator"), None, Some("operator"));
    assert!(
        !allowed(&engine, &unverified, "Issue", "AgentCredential"),
        "an unverified operator claim may not Issue"
    );
}

#[test]
fn reading_agent_credentials_and_types_stays_with_the_tenant() {
    let engine = AuthzEngine::permissive();
    for resource in ["AgentCredential", "AgentType"] {
        for action in ["read", "list"] {
            assert!(
                allowed(&engine, &agent("worker-1", "worker"), action, resource),
                "{action} {resource} is the tenant's to decide"
            );
        }
    }
}

#[test]
fn adapter_delegation_requires_a_tenant_permit_and_respects_forbids() {
    let caller = agent("runner", "worker");
    let empty = AuthzEngine::empty();
    assert!(!allowed(
        &empty,
        &caller,
        "Delegate",
        "AdapterCredentialDelegation"
    ));
    let permitted = AuthzEngine::permissive();
    assert!(allowed(
        &permitted,
        &caller,
        "Delegate",
        "AdapterCredentialDelegation"
    ));
    assert!(!allowed(&permitted, &caller, "Delegate", "AgentType"));
    assert!(!allowed(&permitted, &caller, "Delegate", "AgentCredential"));
    let forbidden = AuthzEngine::new(
        r#"
        permit(principal, action, resource);
        forbid(principal, action == Action::"Delegate", resource is AdapterCredentialDelegation);
        "#,
    )
    .expect("delegation forbid parses");
    assert!(!allowed(
        &forbidden,
        &caller,
        "Delegate",
        "AdapterCredentialDelegation"
    ));
}

#[test]
fn adapter_delegation_is_scoped_to_the_requested_type_and_tenant() {
    let engine = AuthzEngine::empty();
    engine
        .reload_tenant_policies(
            "default",
            r#"
            permit(principal == Agent::"runner", action == Action::"Delegate",
                   resource is AdapterCredentialDelegation)
            when { resource.agent_type_id == "worker-type" };
            "#,
        )
        .expect("scoped delegation policy parses");
    let caller = agent("runner", "worker");
    for (tenant, agent_type, expected) in [
        ("default", "worker-type", true),
        ("default", "operator-type", false),
        ("other-tenant", "worker-type", false),
    ] {
        let attrs = HashMap::from([
            (
                "id".to_string(),
                serde_json::json!("prospective-credential"),
            ),
            ("agent_type_id".to_string(), serde_json::json!(agent_type)),
        ]);
        let decision = engine.authorize_for_tenant_or_bypass(
            tenant,
            &caller,
            "Delegate",
            "AdapterCredentialDelegation",
            &attrs,
        );
        assert_eq!(decision.is_allowed(), expected, "{tenant}/{agent_type}");
    }
}
