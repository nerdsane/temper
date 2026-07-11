//! Cedar request construction and evaluation.

use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::time::Instant;

use cedar_policy::{
    Context, Decision, Entities, Entity, EntityId, EntityTypeName, EntityUid, Request,
    Response as CedarResponse,
};

use super::{AuthzDecision, AuthzEngine, CompiledPolicies};
use crate::context::{PrincipalKind, SecurityContext};
use crate::error::AuthzDenial;
use crate::metrics::{CedarDecisionMetric, CedarPhaseOutcome};

impl AuthzEngine {
    /// Core Cedar evaluation logic shared by both `authorize` and
    /// `authorize_for_tenant`.
    pub(super) fn evaluate_request(
        &self,
        security_ctx: &SecurityContext,
        action: &str,
        resource_type: &str,
        resource_attrs: &HashMap<String, serde_json::Value>,
        policies: &CompiledPolicies,
    ) -> AuthzDecision {
        let mut recorder = CedarEvaluationRecorder::start();

        // Build Cedar principal
        let principal_type = match security_ctx.principal.kind {
            PrincipalKind::Customer => "Customer",
            PrincipalKind::Agent => "Agent",
            PrincipalKind::Admin => "Admin",
            PrincipalKind::System => "System",
        };

        let principal_type = EntityTypeName::from_str(principal_type)
            .expect("fixed principal kinds must be valid Cedar type names");
        let principal_uid = EntityUid::from_type_name_and_id(
            principal_type,
            EntityId::new(&security_ctx.principal.id),
        );
        recorder.finish_phase("principal_uid");

        // Build Cedar action
        let action_uid = EntityUid::from_type_name_and_id(
            EntityTypeName::from_str("Action").expect("Action is a valid Cedar type name"),
            EntityId::new(action),
        );
        recorder.finish_phase("action_uid");

        // Build Cedar resource
        let resource_type = match EntityTypeName::from_str(resource_type) {
            Ok(resource_type) => resource_type,
            Err(e) => {
                return recorder.fail("resource_uid", AuthzDenial::InvalidResource(e.to_string()));
            }
        };
        let resource_uid = EntityUid::from_type_name_and_id(
            resource_type,
            EntityId::new(resource_id_from_attrs(resource_attrs)),
        );
        recorder.finish_phase("resource_uid");

        // Build Cedar context from request-authority facts plus the legacy
        // flattened view of resource attributes. Identity/session names are
        // reserved: an entity field must never impersonate an authenticated
        // request fact (for example, `agentTypeVerified: true`). Resource
        // entities still retain every field through `resource.<field>`.
        let mut ctx_map: HashMap<String, cedar_policy::RestrictedExpression> = HashMap::new();

        for (key, value) in resource_attrs {
            if !is_authority_context_key(key) {
                insert_json_as_cedar(&mut ctx_map, key.clone(), value);
            }
        }

        // Add principal attributes to context
        if let Some(ref role) = security_ctx.principal.role {
            ctx_map.insert(
                "role".to_string(),
                cedar_policy::RestrictedExpression::new_string(role.clone()),
            );
        }
        if let Some(ref acting_for) = security_ctx.principal.acting_for {
            ctx_map.insert(
                "actingFor".to_string(),
                cedar_policy::RestrictedExpression::new_string(acting_for.clone()),
            );
        }

        // Add context attributes
        for (key, value) in &security_ctx.context_attrs {
            insert_json_as_cedar(&mut ctx_map, key.clone(), value);
        }

        crate::metrics::record_cedar_request_attribute_count("context", ctx_map.len());

        // Build context and request
        let context = match Context::from_pairs(ctx_map) {
            Ok(c) => c,
            Err(e) => {
                return recorder.fail("context_attrs", AuthzDenial::InvalidContext(e.to_string()));
            }
        };
        recorder.finish_phase("context_attrs");

        // Build principal entity with attributes so Cedar can resolve both
        // exact UID matches (`principal == Agent::"bot-1"`) and attribute
        // access (`principal.agent_type in [...]`).
        let mut principal_attrs: HashMap<String, cedar_policy::RestrictedExpression> =
            HashMap::new();
        // Extension attributes are loaded first so canonical identity facts
        // below cannot be replaced by a colliding arbitrary attribute.
        for (key, value) in &security_ctx.principal.attributes {
            insert_json_as_cedar(&mut principal_attrs, key.clone(), value);
        }
        principal_attrs.insert(
            "id".to_string(),
            cedar_policy::RestrictedExpression::new_string(security_ctx.principal.id.clone()),
        );
        principal_attrs.insert(
            "accountId".to_string(),
            cedar_policy::RestrictedExpression::new_string(security_ctx.principal.id.clone()),
        );
        if let Some(ref agent_type) = security_ctx.principal.agent_type {
            principal_attrs.insert(
                "agent_type".to_string(),
                cedar_policy::RestrictedExpression::new_string(agent_type.clone()),
            );
        }
        if let Some(ref role) = security_ctx.principal.role {
            principal_attrs.insert(
                "role".to_string(),
                cedar_policy::RestrictedExpression::new_string(role.clone()),
            );
        }
        let agent_type_verified = security_ctx
            .context_attrs
            .get("agentTypeVerified")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        principal_attrs.insert(
            "agentTypeVerified".to_string(),
            cedar_policy::RestrictedExpression::new_bool(agent_type_verified),
        );
        crate::metrics::record_cedar_request_attribute_count("principal", principal_attrs.len());
        recorder.finish_phase("principal_attrs");

        let mut resource_entity_attrs: HashMap<String, cedar_policy::RestrictedExpression> =
            HashMap::new();
        for (key, value) in resource_attrs {
            insert_json_as_cedar(&mut resource_entity_attrs, key.clone(), value);
        }
        crate::metrics::record_cedar_request_attribute_count(
            "resource",
            resource_entity_attrs.len(),
        );
        recorder.finish_phase("resource_attrs");

        // Entity schema validation is intentionally None: app specs define
        // tenant-specific attributes that cannot be predicted by a static
        // platform schema. Policy-level type checks still apply.
        let principal_entity = match Entity::new(
            principal_uid.clone(),
            principal_attrs.clone(),
            HashSet::new(),
        ) {
            Ok(entity) => entity,
            Err(e) => {
                return recorder.fail(
                    "entities",
                    AuthzDenial::EngineError(format!("failed to build principal entity: {e}")),
                );
            }
        };
        let resource_entity = if resource_uid == principal_uid {
            // A resource can share the principal UID. Preserve all resource
            // fields, but canonical principal facts win on name collisions.
            let mut merged_attrs = resource_entity_attrs;
            merged_attrs.extend(principal_attrs);
            match Entity::new(resource_uid.clone(), merged_attrs, HashSet::new()) {
                Ok(entity) => entity,
                Err(e) => {
                    return recorder.fail(
                        "entities",
                        AuthzDenial::EngineError(format!(
                            "failed to build merged principal/resource entity: {e}"
                        )),
                    );
                }
            }
        } else {
            match Entity::new(resource_uid.clone(), resource_entity_attrs, HashSet::new()) {
                Ok(entity) => entity,
                Err(e) => {
                    return recorder.fail(
                        "entities",
                        AuthzDenial::EngineError(format!("failed to build resource entity: {e}")),
                    );
                }
            }
        };

        let entities = if resource_uid == principal_uid {
            match Entities::from_entities([resource_entity], None) {
                Ok(e) => e,
                Err(e) => {
                    return recorder.fail(
                        "entities",
                        AuthzDenial::EngineError(format!("failed to build entity store: {e}")),
                    );
                }
            }
        } else {
            match Entities::from_entities([principal_entity, resource_entity], None) {
                Ok(e) => e,
                Err(e) => {
                    return recorder.fail(
                        "entities",
                        AuthzDenial::EngineError(format!("failed to build entity store: {e}")),
                    );
                }
            }
        };
        recorder.finish_phase("entities");

        let request = match Request::new(
            principal_uid.clone(),
            action_uid.clone(),
            resource_uid.clone(),
            context,
            None, // schema-less: actions/resources are tenant-defined
        ) {
            Ok(r) => r,
            Err(e) => {
                return recorder.fail(
                    "request",
                    AuthzDenial::EngineError(format!("invalid request: {e}")),
                );
            }
        };
        recorder.finish_phase("request");

        let candidate_selection =
            policies
                .candidate_index
                .select(&principal_uid, &action_uid, &resource_uid);
        crate::metrics::record_cedar_policy_candidate_counts(
            candidate_selection.counts.full,
            candidate_selection.counts.candidate,
            candidate_selection.counts.outcome.as_str(),
        );
        recorder.finish_phase("policy_candidates");

        let effective_policy_set = candidate_selection
            .policy_set
            .as_ref()
            .unwrap_or(&policies.policy_set);
        let response: CedarResponse =
            self.authorizer
                .is_authorized(&request, effective_policy_set, &entities);
        recorder.finish_phase("authorizer");

        let decision = response.decision();
        recorder.finish(match decision {
            Decision::Allow => CedarDecisionMetric::Allow,
            Decision::Deny => CedarDecisionMetric::Deny,
        });

        match decision {
            Decision::Allow => {
                let policy_ids: Vec<String> = response
                    .diagnostics()
                    .reason()
                    .map(|id| id.to_string())
                    .collect();
                AuthzDecision::Allow { policy_ids }
            }
            Decision::Deny => {
                let policy_ids: Vec<String> = response
                    .diagnostics()
                    .reason()
                    .map(|id| id.to_string())
                    .collect();
                if policy_ids.is_empty() {
                    AuthzDecision::Deny(AuthzDenial::NoMatchingPermit)
                } else {
                    AuthzDecision::Deny(AuthzDenial::PolicyDenied { policy_ids })
                }
            }
        }
    }
}

struct CedarEvaluationRecorder {
    started_at: Instant,
    phase_started_at: Instant,
}

impl CedarEvaluationRecorder {
    fn start() -> Self {
        let now = Instant::now();
        Self {
            started_at: now,
            phase_started_at: now,
        }
    }

    fn finish_phase(&mut self, phase: &'static str) {
        crate::metrics::record_cedar_phase_duration(
            phase,
            self.phase_started_at.elapsed(),
            CedarPhaseOutcome::Ok,
        );
        self.phase_started_at = Instant::now();
    }

    fn fail(&mut self, phase: &'static str, denial: AuthzDenial) -> AuthzDecision {
        crate::metrics::record_cedar_phase_duration(
            phase,
            self.phase_started_at.elapsed(),
            CedarPhaseOutcome::Error,
        );
        crate::metrics::record_cedar_evaluation(
            self.started_at.elapsed(),
            CedarDecisionMetric::Error,
        );
        AuthzDecision::Deny(denial)
    }

    fn finish(&self, decision: CedarDecisionMetric) {
        crate::metrics::record_cedar_evaluation(self.started_at.elapsed(), decision);
    }
}

/// Insert a `serde_json::Value` into a Cedar context map, converting to the
/// appropriate `RestrictedExpression` type. Supports strings, bools, integers,
/// and arrays of those types.
fn insert_json_as_cedar(
    map: &mut HashMap<String, cedar_policy::RestrictedExpression>,
    key: String,
    value: &serde_json::Value,
) {
    if let Some(s) = value.as_str() {
        map.insert(
            key,
            cedar_policy::RestrictedExpression::new_string(s.to_string()),
        );
    } else if let Some(b) = value.as_bool() {
        map.insert(key, cedar_policy::RestrictedExpression::new_bool(b));
    } else if let Some(n) = value.as_i64() {
        map.insert(key, cedar_policy::RestrictedExpression::new_long(n));
    } else if let Some(arr) = value.as_array() {
        let items: Vec<cedar_policy::RestrictedExpression> = arr
            .iter()
            .filter_map(|item| {
                if let Some(s) = item.as_str() {
                    Some(cedar_policy::RestrictedExpression::new_string(
                        s.to_string(),
                    ))
                } else if let Some(n) = item.as_i64() {
                    Some(cedar_policy::RestrictedExpression::new_long(n))
                } else {
                    item.as_bool()
                        .map(cedar_policy::RestrictedExpression::new_bool)
                }
            })
            .collect();
        map.insert(key, cedar_policy::RestrictedExpression::new_set(items));
    }
}

/// Request-authority attributes that must never be sourced from resource
/// state when building the legacy flat Cedar context.
fn is_authority_context_key(key: &str) -> bool {
    matches!(
        key,
        "role" | "actingFor" | "agentId" | "agentType" | "agentTypeVerified" | "sessionId"
    )
}

fn resource_id_from_attrs(attrs: &HashMap<String, serde_json::Value>) -> String {
    attrs
        .get("id")
        .or_else(|| attrs.get("Id"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string()
}
