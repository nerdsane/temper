//! Cedar-backed authorization gate for WASM host functions.
//!
//! `CedarWasmAuthzGate` translates WASM host function calls into Cedar
//! authorization requests. `PermissiveWasmAuthzGate` allows everything
//! (used when Cedar WASM gating is not configured or in tests).

use std::collections::BTreeMap;
use std::sync::Arc;

use temper_authz::{AuthzDecision, AuthzEngine, PrincipalKind, SecurityContext};
use temper_wasm::{WasmAuthzContext, WasmAuthzDecision, WasmAuthzGate};

/// Cedar-backed authorization gate for WASM host function calls.
///
/// Translates `http_call` and `get_secret` into Cedar authorization requests:
///
/// | Concept    | http_call                    | access_secret           |
/// |------------|------------------------------|-------------------------|
/// | Principal  | `Agent::"module_name"`       | Same                    |
/// | Action     | `Action::"http_call"`        | `Action::"access_secret"` |
/// | Resource   | `HttpEndpoint::"domain"`     | `Secret::"key_name"`    |
/// | Context    | method, url, tenant, etc.    | tenant, module, agent   |
pub struct CedarWasmAuthzGate {
    /// The Cedar authorization engine.
    engine: Arc<AuthzEngine>,
}

impl CedarWasmAuthzGate {
    /// Create a new Cedar-backed gate.
    pub fn new(engine: Arc<AuthzEngine>) -> Self {
        Self { engine }
    }
}

impl WasmAuthzGate for CedarWasmAuthzGate {
    fn authorize_http_call(
        &self,
        domain: &str,
        method: &str,
        url: &str,
        ctx: &WasmAuthzContext,
    ) -> WasmAuthzDecision {
        let security_ctx = build_wasm_security_context(ctx);

        // Build resource attrs with BTreeMap (DST compliant)
        let mut resource_attrs: BTreeMap<String, serde_json::Value> = BTreeMap::new();
        resource_attrs.insert("id".into(), serde_json::Value::String(domain.to_string()));
        resource_attrs.insert(
            "domain".into(),
            serde_json::Value::String(domain.to_string()),
        );

        // Add method and url to the security context attrs for Cedar conditions
        let mut enriched_ctx = security_ctx;
        enriched_ctx.context_attrs.insert(
            "method".into(),
            serde_json::Value::String(method.to_uppercase()),
        );
        enriched_ctx
            .context_attrs
            .insert("url".into(), serde_json::Value::String(url.to_string()));

        // Convert BTreeMap to HashMap at Cedar boundary (determinism-ok)
        let hash_attrs: std::collections::HashMap<_, _> = resource_attrs.into_iter().collect(); // determinism-ok: Cedar API requires HashMap
        let decision = self.engine.authorize_for_tenant_or_bypass(
            &ctx.tenant,
            &enriched_ctx,
            "http_call",
            "HttpEndpoint",
            &hash_attrs,
        );

        match decision {
            AuthzDecision::Allow { .. } => WasmAuthzDecision::Allow,
            AuthzDecision::Deny(denial) => WasmAuthzDecision::Deny(denial.to_string()),
        }
    }

    fn authorize_secret_access(
        &self,
        secret_key: &str,
        ctx: &WasmAuthzContext,
    ) -> WasmAuthzDecision {
        let security_ctx = build_wasm_security_context(ctx);

        let mut resource_attrs: BTreeMap<String, serde_json::Value> = BTreeMap::new();
        resource_attrs.insert(
            "id".into(),
            serde_json::Value::String(secret_key.to_string()),
        );

        // Convert BTreeMap to HashMap at Cedar boundary (determinism-ok)
        let hash_attrs: std::collections::HashMap<_, _> = resource_attrs.into_iter().collect(); // determinism-ok: Cedar API requires HashMap
        let decision = self.engine.authorize_for_tenant_or_bypass(
            &ctx.tenant,
            &security_ctx,
            "access_secret",
            "Secret",
            &hash_attrs,
        );

        match decision {
            AuthzDecision::Allow { .. } => WasmAuthzDecision::Allow,
            AuthzDecision::Deny(denial) => WasmAuthzDecision::Deny(denial.to_string()),
        }
    }
}

/// Permissive gate that allows all WASM host function calls.
///
/// Used when Cedar WASM gating is not configured, preserving backward
/// compatibility with existing ungated behavior.
pub struct PermissiveWasmAuthzGate;

impl WasmAuthzGate for PermissiveWasmAuthzGate {
    fn authorize_http_call(
        &self,
        _domain: &str,
        _method: &str,
        _url: &str,
        _ctx: &WasmAuthzContext,
    ) -> WasmAuthzDecision {
        WasmAuthzDecision::Allow
    }

    fn authorize_secret_access(&self, _key: &str, _ctx: &WasmAuthzContext) -> WasmAuthzDecision {
        WasmAuthzDecision::Allow
    }
}

/// Build a `SecurityContext` from a `WasmAuthzContext`.
///
/// The principal is the WASM module (kind=Agent, role=wasm_module).
fn build_wasm_security_context(ctx: &WasmAuthzContext) -> SecurityContext {
    let mut context_attrs = std::collections::HashMap::new(); // determinism-ok: SecurityContext uses HashMap
    context_attrs.insert(
        "tenant".into(),
        serde_json::Value::String(ctx.tenant.clone()),
    );
    context_attrs.insert(
        "module".into(),
        serde_json::Value::String(ctx.module_name.clone()),
    );
    context_attrs.insert(
        "entityType".into(),
        serde_json::Value::String(ctx.entity_type.clone()),
    );
    context_attrs.insert(
        "triggerAction".into(),
        serde_json::Value::String(ctx.trigger_action.clone()),
    );
    if let Some(ref aid) = ctx.agent_id {
        context_attrs.insert("agentId".into(), serde_json::Value::String(aid.clone()));
    }

    SecurityContext {
        principal: temper_authz::Principal {
            id: ctx.module_name.clone(),
            kind: PrincipalKind::Agent,
            role: Some("wasm_module".to_string()),
            acting_for: None,
            agent_type: None,
            attributes: std::collections::HashMap::new(), // determinism-ok: Principal uses HashMap
        },
        context_attrs,
        correlation_id: uuid::Uuid::now_v7().to_string(), // determinism-ok: correlation_id for Cedar tracing only
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ctx() -> WasmAuthzContext {
        WasmAuthzContext::test_fixture()
    }

    #[test]
    fn permissive_gate_allows_all() {
        let gate = PermissiveWasmAuthzGate;
        let ctx = test_ctx();

        assert_eq!(
            gate.authorize_http_call(
                "api.stripe.com",
                "POST",
                "https://api.stripe.com/v1/charges",
                &ctx
            ),
            WasmAuthzDecision::Allow,
        );
        assert_eq!(
            gate.authorize_secret_access("STRIPE_API_KEY", &ctx),
            WasmAuthzDecision::Allow,
        );
    }

    #[test]
    fn cedar_gate_denies_without_policies() {
        // No policies = no permits = deny (Cedar default-deny semantics)
        let engine = Arc::new(AuthzEngine::empty());
        let gate = CedarWasmAuthzGate::new(engine);
        let ctx = test_ctx();

        // Agent principal is not System, so no bypass — Cedar denies.
        let result = gate.authorize_http_call(
            "api.stripe.com",
            "POST",
            "https://api.stripe.com/v1/charges",
            &ctx,
        );
        assert_eq!(
            result,
            WasmAuthzDecision::Deny("no matching permit policy".into()),
        );
    }

    #[test]
    fn cedar_gate_allows_with_matching_policy() {
        let policy = r#"
            permit(
                principal is Agent,
                action == Action::"http_call",
                resource is HttpEndpoint
            ) when {
                context.module == "stripe_charge"
            };
        "#;
        let engine = Arc::new(AuthzEngine::new(policy).unwrap());
        let gate = CedarWasmAuthzGate::new(engine);
        let ctx = test_ctx();

        let result = gate.authorize_http_call(
            "api.stripe.com",
            "POST",
            "https://api.stripe.com/v1/charges",
            &ctx,
        );
        assert_eq!(result, WasmAuthzDecision::Allow);
    }

    #[test]
    fn cedar_gate_secret_with_matching_policy() {
        let policy = r#"
            permit(
                principal is Agent,
                action == Action::"access_secret",
                resource is Secret
            ) when {
                context.module == "stripe_charge"
            };
        "#;
        let engine = Arc::new(AuthzEngine::new(policy).unwrap());
        let gate = CedarWasmAuthzGate::new(engine);
        let ctx = test_ctx();

        assert_eq!(
            gate.authorize_secret_access("STRIPE_API_KEY", &ctx),
            WasmAuthzDecision::Allow,
        );
    }

    #[test]
    fn cedar_gate_denies_wrong_module() {
        let policy = r#"
            permit(
                principal is Agent,
                action == Action::"http_call",
                resource is HttpEndpoint
            ) when {
                context.module == "email_sender"
            };
        "#;
        let engine = Arc::new(AuthzEngine::new(policy).unwrap());
        let gate = CedarWasmAuthzGate::new(engine);
        let ctx = test_ctx(); // module_name = "stripe_charge"

        let result = gate.authorize_http_call(
            "api.stripe.com",
            "POST",
            "https://api.stripe.com/v1/charges",
            &ctx,
        );
        assert!(matches!(result, WasmAuthzDecision::Deny(_)));
    }

    #[test]
    fn cedar_gate_uses_tenant_scoped_policies() {
        let engine = Arc::new(AuthzEngine::empty());
        let policy = r#"
            permit(
                principal is Agent,
                action == Action::"http_call",
                resource is HttpEndpoint
            ) when {
                context.module == "stripe_charge" &&
                context.domain == "api.stripe.com"
            };
        "#;
        engine
            .reload_tenant_policies("test-tenant", policy)
            .expect("tenant policy should load");

        let gate = CedarWasmAuthzGate::new(engine);
        let ctx = test_ctx(); // tenant = test-tenant, module_name = stripe_charge
        let result = gate.authorize_http_call(
            "api.stripe.com",
            "POST",
            "https://api.stripe.com/v1/charges",
            &ctx,
        );
        assert_eq!(result, WasmAuthzDecision::Allow);
    }

    #[test]
    fn build_security_context_from_wasm_ctx() {
        let ctx = test_ctx();
        let sec_ctx = build_wasm_security_context(&ctx);

        assert_eq!(sec_ctx.principal.id, "stripe_charge");
        assert_eq!(sec_ctx.principal.kind, PrincipalKind::Agent);
        assert_eq!(sec_ctx.principal.role, Some("wasm_module".to_string()));
        assert_eq!(
            sec_ctx.context_attrs.get("tenant"),
            Some(&serde_json::Value::String("test-tenant".into())),
        );
        assert_eq!(
            sec_ctx.context_attrs.get("module"),
            Some(&serde_json::Value::String("stripe_charge".into())),
        );
    }
}
