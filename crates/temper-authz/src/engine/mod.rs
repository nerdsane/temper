//! Cedar policy evaluation engine.
//!
//! Wraps the cedar-policy crate to provide authorization decisions
//! for OData operations. Translates Temper concepts (entities, actions,
//! security contexts) into Cedar's authorization model.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::str::FromStr;
use std::sync::RwLock;
use std::time::Instant;

use cedar_policy::{
    Authorizer, Context, Decision, Entities, Entity, EntityUid, PolicyId, PolicySet, Request,
    Response as CedarResponse,
};

use crate::context::{PrincipalKind, SecurityContext};
use crate::error::{AuthzDenial, AuthzError};
use crate::metrics::{CedarDecisionMetric, CedarPhaseOutcome};

mod candidates;

#[cfg(test)]
#[path = "engine_test.rs"]
mod tests;

/// The result of an authorization check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthzDecision {
    /// The request is allowed, with the policy IDs that contributed to the permit.
    Allow { policy_ids: Vec<String> },
    /// The request is denied with typed denial details.
    Deny(AuthzDenial),
}

impl AuthzDecision {
    /// Returns `true` if the authorization decision is `Allow`.
    pub fn is_allowed(&self) -> bool {
        matches!(self, AuthzDecision::Allow { .. })
    }

    /// Returns the denial details if the decision is `Deny`.
    pub fn denial(&self) -> Option<&AuthzDenial> {
        match self {
            AuthzDecision::Allow { .. } => None,
            AuthzDecision::Deny(d) => Some(d),
        }
    }

    /// Returns the policy IDs that contributed to the allow decision.
    pub fn policy_ids(&self) -> &[String] {
        match self {
            AuthzDecision::Allow { policy_ids } => policy_ids,
            AuthzDecision::Deny(_) => &[],
        }
    }
}

/// Compiled policy data used by Cedar and the request-time candidate selector.
struct CompiledPolicies {
    policy_set: PolicySet,
    candidate_index: candidates::CandidatePolicyIndex,
}

impl CompiledPolicies {
    fn new(policy_set: PolicySet) -> Self {
        let candidate_index = candidates::CandidatePolicyIndex::new(&policy_set);
        Self {
            policy_set,
            candidate_index,
        }
    }
}

/// A labelled chunk of Cedar policy text: `(label, cedar_text)`.
///
/// The label names where the text came from — `"katagami-commons/art_style.cedar"`
/// — and becomes the prefix of every `PolicyId` parsed out of it, so a denial
/// reads `denied by policy: katagami-commons/art_style.cedar#2` instead of
/// `policy1874`. See [`policy_statement_id`].
pub type PolicySource = (String, String);

/// Build the `PolicyId` for the `index`-th statement (0-based) of `label`.
///
/// Rendered as `"{label}#{n}"` with `n` 1-based, so `#2` names the second
/// `permit`/`forbid` statement in the file a human would open. Every statement
/// is suffixed, including the only one in a single-statement file, so an
/// existing id does not shift meaning when a sibling statement is added.
fn policy_statement_id(label: &str, index: usize) -> PolicyId {
    PolicyId::new(format!("{label}#{}", index + 1))
}

/// Label for a tenant's carried-over unlabelled policy blob.
///
/// Statements inside it are still positional, but the id at least says they
/// came from the tenant blob rather than a named file. The label disappears
/// once the apps owning those statements have been loaded from labelled
/// sources. Every caller that carries a blob forward uses this one label, so a
/// tenant never accumulates two differently-named copies of the same text.
pub fn unlabelled_source_label(tenant: &str) -> String {
    format!("{tenant}:unlabelled")
}

/// Per-tenant policy data: the compiled policies and the source text.
struct TenantPolicies {
    policies: CompiledPolicies,
    source_text: String,
    /// The labelled sources this policy set was built from, in load order.
    /// Empty when the tenant was loaded from an unlabelled blob via
    /// [`AuthzEngine::reload_tenant_policies`]. Callers that rebuild a
    /// tenant's policy set — an os-app install, say — read this back so
    /// provenance established by earlier loads is not flattened away.
    sources: Vec<PolicySource>,
}

/// Compile labelled sources into a tenant's policy set.
///
/// Each `(label, cedar_text)` pair is parsed on its own and every statement it
/// contains is re-keyed to `"{label}#{n}"` (`n` 1-based) — see
/// [`AuthzEngine::reload_tenant_policies_named`] for why the labels matter.
///
/// Takes no locks, so a caller holding the tenant write lock across a
/// read-modify-write can compile inside that critical section.
fn compile_tenant_policies(policies: &[PolicySource]) -> Result<TenantPolicies, AuthzError> {
    let mut combined_set = PolicySet::new();
    let mut combined_text = String::new();

    for (label, cedar_text) in policies {
        // Parse each entry's Cedar text individually.
        let entry_set: PolicySet = cedar_text
            .parse()
            .map_err(|e| AuthzError::PolicyParse(format!("{label}: {e}")))?;

        // Re-add each statement under an id that names its source.
        for (idx, policy) in entry_set.policies().enumerate() {
            let named = policy.clone().new_id(policy_statement_id(label, idx));
            combined_set
                .add(named)
                .map_err(|e| AuthzError::PolicyParse(format!("{label}: {e}")))?;
        }

        if !combined_text.is_empty() {
            combined_text.push('\n');
        }
        combined_text.push_str(cedar_text);
    }

    merge_system_platform_policy(&mut combined_set);

    Ok(TenantPolicies {
        policies: CompiledPolicies::new(combined_set),
        source_text: combined_text,
        sources: policies.to_vec(),
    })
}

/// A tenant's labelled sources, or its unlabelled blob as a single entry.
///
/// Takes the tenant's entry rather than the engine so it can be called with
/// the tenant lock already held.
fn tenant_sources_or_blob(tenant: &str, existing: Option<&TenantPolicies>) -> Vec<PolicySource> {
    let Some(tp) = existing else {
        return Vec::new();
    };
    if !tp.sources.is_empty() {
        return tp.sources.clone();
    }
    if tp.source_text.trim().is_empty() {
        return Vec::new();
    }
    vec![(unlabelled_source_label(tenant), tp.source_text.clone())]
}

/// The authorization engine. Holds per-tenant compiled Cedar policies and
/// evaluates authorization requests. Supports hot-reload of policies via
/// [`reload_tenant_policies`](AuthzEngine::reload_tenant_policies).
///
/// Uses `BTreeMap` for deterministic iteration order (DST compliance).
pub struct AuthzEngine {
    /// Per-tenant policy sets. Each tenant has its own isolated PolicySet.
    tenant_policies: RwLock<BTreeMap<String, TenantPolicies>>,
    /// Fallback global policy set for callers that don't specify a tenant.
    /// Deprecated: callers should migrate to `authorize_for_tenant`.
    fallback_policy_set: RwLock<CompiledPolicies>,
    authorizer: Authorizer,
}

impl AuthzEngine {
    /// Create a new AuthzEngine from Cedar policy text (loaded into the
    /// fallback global policy set). ADR-0046: the built-in `system-platform`
    /// policy is merged in so System principals are authorized by an
    /// explicit, auditable policy rather than a hard-coded bypass.
    pub fn new(policy_text: &str) -> Result<Self, AuthzError> {
        // Parse the user policy first — return an error if it's malformed.
        let mut policy_set = policy_text
            .parse::<PolicySet>()
            .map_err(|e| AuthzError::PolicyParse(e.to_string()))?;
        merge_system_platform_policy(&mut policy_set);

        Ok(Self {
            tenant_policies: RwLock::new(BTreeMap::new()),
            fallback_policy_set: RwLock::new(CompiledPolicies::new(policy_set)),
            authorizer: Authorizer::new(),
        })
    }

    /// Create an AuthzEngine with no user policies, but with the built-in
    /// `system-platform` policy installed (ADR-0046). System principals
    /// remain authorized; everything else hits Cedar's default-deny.
    ///
    /// Use this to test deny behavior for non-System principals. For test
    /// setups that need all requests to be allowed, use
    /// [`permissive`](Self::permissive) instead.
    pub fn empty() -> Self {
        let mut policy_set = PolicySet::new();
        merge_system_platform_policy(&mut policy_set);
        Self {
            tenant_policies: RwLock::new(BTreeMap::new()),
            fallback_policy_set: RwLock::new(CompiledPolicies::new(policy_set)),
            authorizer: Authorizer::new(),
        }
    }

    /// Create an AuthzEngine that permits all requests.
    ///
    /// Loads a single catch-all `permit(principal, action, resource);` policy
    /// so that Cedar evaluates to Allow for every principal kind (System or
    /// otherwise). Used in tests and permissive dev environments.
    pub fn permissive() -> Self {
        let policy_set =
            PolicySet::from_str("permit(principal, action, resource);").unwrap_or_default();
        Self {
            tenant_policies: RwLock::new(BTreeMap::new()),
            fallback_policy_set: RwLock::new(CompiledPolicies::new(policy_set)),
            authorizer: Authorizer::new(),
        }
    }

    /// Hot-reload Cedar policies for a specific tenant. Parses and validates
    /// the new policy text, then atomically swaps the tenant's policy set.
    /// If parsing fails, existing policies remain in effect.
    pub fn reload_tenant_policies(
        &self,
        tenant: &str,
        policy_text: &str,
    ) -> Result<(), AuthzError> {
        let mut new_policy_set = policy_text
            .parse::<PolicySet>()
            .map_err(|e| AuthzError::PolicyParse(e.to_string()))?;
        merge_system_platform_policy(&mut new_policy_set);

        let mut tenants = self
            .tenant_policies
            .write()
            .map_err(|e| AuthzError::Engine(format!("tenant policy lock poisoned: {e}")))?;

        tenants.insert(
            tenant.to_string(),
            TenantPolicies {
                policies: CompiledPolicies::new(new_policy_set),
                source_text: policy_text.to_string(),
                sources: Vec::new(),
            },
        );
        Ok(())
    }

    /// Hot-reload Cedar policies for a tenant from labelled sources.
    ///
    /// Each `(label, cedar_text)` pair is parsed on its own and every statement
    /// it contains is re-keyed to `"{label}#{n}"` (`n` 1-based). Labelling the
    /// source is the whole point: Cedar names policies positionally within
    /// whatever text it was handed, so a concatenated tenant blob yields ids
    /// like `policy1874` that map back to nothing, and a denial diagnostic
    /// becomes unreadable (ARN-286). With a `"{app}/{file}.cedar"` label the
    /// same denial reads `denied by policy: katagami-commons/art_style.cedar#2`.
    ///
    /// The labelled sources are retained so a later caller can rebuild the
    /// tenant's set without losing provenance — see [`tenant_policy_sources`](Self::tenant_policy_sources).
    pub fn reload_tenant_policies_named(
        &self,
        tenant: &str,
        policies: &[PolicySource],
    ) -> Result<(), AuthzError> {
        let compiled = compile_tenant_policies(policies)?;

        let mut tenants = self
            .tenant_policies
            .write()
            .map_err(|e| AuthzError::Engine(format!("tenant policy lock poisoned: {e}")))?;

        tenants.insert(tenant.to_string(), compiled);
        Ok(())
    }

    /// The labelled policy sources currently active for a tenant, in load
    /// order.
    ///
    /// Empty when the tenant's policies were loaded from an unlabelled blob.
    /// Callers rebuilding a tenant's policy set start from this so labels
    /// contributed by earlier loads survive.
    pub fn tenant_policy_sources(&self, tenant: &str) -> Vec<PolicySource> {
        self.tenant_policies
            .read()
            .ok()
            .and_then(|t| t.get(tenant).map(|tp| tp.sources.clone()))
            .unwrap_or_default()
    }

    /// Rebuild a tenant's labelled policy sources under a single lock.
    ///
    /// `update` receives the sources currently active for the tenant — or its
    /// unlabelled blob as one entry — and returns the set to install. Reading
    /// the current sources, applying `update`, compiling the result and
    /// swapping it in all happen while this call holds the tenant write lock,
    /// so two concurrent rebuilds serialize. Splitting that into a read
    /// followed by a separate [`reload_tenant_policies_named`](Self::reload_tenant_policies_named)
    /// loses updates: both callers read the same pre-state, both write, and
    /// the first caller's policies are gone while both calls returned `Ok`.
    ///
    /// Returns the tenant's combined policy text as installed, so a caller
    /// mirroring that text into a cache writes what the engine actually holds
    /// rather than what it computed before the merge.
    ///
    /// `update` runs with the tenant lock held, so it must not call back into
    /// this engine — [`get_tenant_policy_text`](Self::get_tenant_policy_text),
    /// [`tenant_policy_sources`](Self::tenant_policy_sources) or another
    /// update would deadlock. Read anything it needs from the engine before
    /// calling. Cedar compilation also happens under the lock, so a tenant
    /// with a large policy set has its authorization requests briefly queued
    /// behind a rebuild; that is the cost of the sequence being atomic.
    ///
    /// If the updated sources do not parse, the tenant keeps the policies it
    /// had.
    pub fn update_tenant_policy_sources<F>(
        &self,
        tenant: &str,
        update: F,
    ) -> Result<String, AuthzError>
    where
        F: FnOnce(Vec<PolicySource>) -> Vec<PolicySource>,
    {
        let mut tenants = self
            .tenant_policies
            .write()
            .map_err(|e| AuthzError::Engine(format!("tenant policy lock poisoned: {e}")))?;

        let current = tenant_sources_or_blob(tenant, tenants.get(tenant));
        let updated = update(current);
        // Compile before touching the map: a parse failure leaves the tenant
        // on the policies it already had.
        let compiled = compile_tenant_policies(&updated)?;
        let policy_text = compiled.source_text.clone();
        tenants.insert(tenant.to_string(), compiled);
        Ok(policy_text)
    }

    /// Add or replace a single labelled policy source and reload the tenant.
    ///
    /// For paths that contribute one policy to a tenant that already has some
    /// — generating a policy from an approved governance decision, say.
    /// Sources already loaded keep their labels, so adding a policy does not
    /// flatten a tenant back to positional `policy1874` ids (ARN-286). If the
    /// new text does not parse, the tenant's existing policies stay in effect.
    ///
    /// Atomic: two policies upserted concurrently both survive.
    pub fn upsert_tenant_policy_source(
        &self,
        tenant: &str,
        label: &str,
        cedar_text: &str,
    ) -> Result<(), AuthzError> {
        self.update_tenant_policy_sources(tenant, |mut sources| {
            match sources.iter_mut().find(|(existing, _)| existing == label) {
                Some(entry) => entry.1 = cedar_text.to_string(),
                None => sources.push((label.to_string(), cedar_text.to_string())),
            }
            sources
        })?;
        Ok(())
    }

    /// Remove a tenant's policy set entirely.
    pub fn remove_tenant(&self, tenant: &str) {
        if let Ok(mut tenants) = self.tenant_policies.write() {
            tenants.remove(tenant);
        }
    }

    /// Get the combined Cedar policy text for a tenant.
    pub fn get_tenant_policy_text(&self, tenant: &str) -> Option<String> {
        self.tenant_policies
            .read()
            .ok()
            .and_then(|t| t.get(tenant).map(|tp| tp.source_text.clone()))
    }

    /// Hot-reload Cedar policies into the fallback global policy set.
    ///
    /// **Deprecated**: Use [`reload_tenant_policies`](Self::reload_tenant_policies)
    /// for per-tenant isolation. This method exists for backward compatibility
    /// during migration.
    pub fn reload_policies(&self, policy_text: &str) -> Result<(), AuthzError> {
        let mut new_policy_set = policy_text
            .parse::<PolicySet>()
            .map_err(|e| AuthzError::PolicyParse(e.to_string()))?;
        merge_system_platform_policy(&mut new_policy_set);

        let mut current = self
            .fallback_policy_set
            .write()
            .map_err(|e| AuthzError::Engine(format!("policy lock poisoned: {e}")))?;
        *current = CompiledPolicies::new(new_policy_set);
        Ok(())
    }

    /// Returns the total number of policies across all tenants + fallback.
    pub fn policy_count(&self) -> usize {
        let tenant_count: usize = self
            .tenant_policies
            .read()
            .map(|t| {
                t.values()
                    .map(|tp| count_user_policies(&tp.policies.policy_set))
                    .sum()
            })
            .unwrap_or(0);
        let fallback_count = self
            .fallback_policy_set
            .read()
            .map_or(0, |ps| count_user_policies(&ps.policy_set));
        tenant_count + fallback_count
    }

    /// Evaluate an authorization request against the fallback global policy set.
    ///
    /// **Prefer [`authorize_for_tenant`](Self::authorize_for_tenant)** for
    /// per-tenant isolation. This method exists for backward compatibility.
    pub fn authorize(
        &self,
        security_ctx: &SecurityContext,
        action: &str,
        resource_type: &str,
        resource_attrs: &HashMap<String, serde_json::Value>,
    ) -> AuthzDecision {
        let policy_set = match self.fallback_policy_set.read() {
            Ok(ps) => ps,
            Err(e) => {
                return AuthzDecision::Deny(AuthzDenial::EngineError(format!(
                    "policy lock poisoned: {e}"
                )));
            }
        };
        self.evaluate_request(
            security_ctx,
            action,
            resource_type,
            resource_attrs,
            &policy_set,
        )
    }

    /// Evaluate an authorization request against a specific tenant's policy set.
    ///
    /// If the tenant has no policies loaded, falls back to Cedar default-deny
    /// (returns `NoMatchingPermit`).
    pub fn authorize_for_tenant(
        &self,
        tenant: &str,
        security_ctx: &SecurityContext,
        action: &str,
        resource_type: &str,
        resource_attrs: &HashMap<String, serde_json::Value>,
    ) -> AuthzDecision {
        let tenants = match self.tenant_policies.read() {
            Ok(t) => t,
            Err(e) => {
                return AuthzDecision::Deny(AuthzDenial::EngineError(format!(
                    "tenant policy lock poisoned: {e}"
                )));
            }
        };

        if let Some(tp) = tenants.get(tenant) {
            self.evaluate_request(
                security_ctx,
                action,
                resource_type,
                resource_attrs,
                &tp.policies,
            )
        } else {
            // No per-tenant policies loaded — fall back to global.
            drop(tenants);
            self.authorize(security_ctx, action, resource_type, resource_attrs)
        }
    }

    /// Core Cedar evaluation logic shared by both `authorize` and
    /// `authorize_for_tenant`.
    fn evaluate_request(
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

        let principal_uid = match EntityUid::from_str(&format!(
            "{}::\"{}\"",
            principal_type, security_ctx.principal.id
        )) {
            Ok(uid) => uid,
            Err(e) => {
                return recorder.fail(
                    "principal_uid",
                    AuthzDenial::InvalidPrincipal(e.to_string()),
                );
            }
        };
        recorder.finish_phase("principal_uid");

        // Build Cedar action
        let action_uid = match EntityUid::from_str(&format!("Action::\"{}\"", action)) {
            Ok(uid) => uid,
            Err(e) => {
                return recorder.fail("action_uid", AuthzDenial::InvalidAction(e.to_string()));
            }
        };
        recorder.finish_phase("action_uid");

        // Build Cedar resource
        let resource_uid = match EntityUid::from_str(&format!(
            "{}::\"{}\"",
            resource_type,
            resource_id_from_attrs(resource_attrs)
        )) {
            Ok(uid) => uid,
            Err(e) => {
                return recorder.fail("resource_uid", AuthzDenial::InvalidResource(e.to_string()));
            }
        };
        recorder.finish_phase("resource_uid");

        // Build Cedar context from security context attrs + resource attrs
        let mut ctx_map: HashMap<String, cedar_policy::RestrictedExpression> = HashMap::new();

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

        // Inject resource attributes into context (enables Cedar policies to
        // reference entity state and cross-entity context via `context.key`).
        for (key, value) in resource_attrs {
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
        for (key, value) in &security_ctx.principal.attributes {
            insert_json_as_cedar(&mut principal_attrs, key.clone(), value);
        }
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
            let mut merged_attrs = principal_attrs;
            merged_attrs.extend(resource_entity_attrs);
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

    /// Quick check: is this a system principal?
    ///
    /// Since ADR-0046, this no longer bypasses authorization. It is kept as a
    /// convenience predicate for callers that want to branch on principal
    /// kind for reasons other than authorization (logging, telemetry tagging).
    /// Actual authorization of System principals flows through the normal
    /// Cedar evaluation, matching the built-in `system-platform` policy
    /// installed at engine construction time (see `SYSTEM_PLATFORM_POLICY`).
    pub fn is_system(security_ctx: &SecurityContext) -> bool {
        security_ctx.principal.kind == PrincipalKind::System
    }

    /// Authorize through the fallback global policy set.
    ///
    /// ADR-0046: formerly short-circuited System principals with an unchecked
    /// Allow. System authority is now explicit in the `system-platform` Cedar
    /// policy; delegating straight to [`authorize`] ensures every request is
    /// policy-checked and logged.
    pub fn authorize_or_bypass(
        &self,
        security_ctx: &SecurityContext,
        action: &str,
        resource_type: &str,
        resource_attrs: &HashMap<String, serde_json::Value>,
    ) -> AuthzDecision {
        self.authorize(security_ctx, action, resource_type, resource_attrs)
    }

    /// Authorize for a specific tenant through Cedar.
    ///
    /// ADR-0046: formerly short-circuited System principals with an unchecked
    /// Allow. System authority is now explicit in the `system-platform`
    /// policy merged into the fallback policy set; this function simply
    /// delegates to [`authorize_for_tenant`].
    pub fn authorize_for_tenant_or_bypass(
        &self,
        tenant: &str,
        security_ctx: &SecurityContext,
        action: &str,
        resource_type: &str,
        resource_attrs: &HashMap<String, serde_json::Value>,
    ) -> AuthzDecision {
        self.authorize_for_tenant(tenant, security_ctx, action, resource_type, resource_attrs)
    }
}

/// Built-in Cedar policy granting System-kind principals broad authority
/// (ADR-0046). Installed into every [`AuthzEngine`] at construction time so
/// that platform code paths using `AgentContext::system()` continue to
/// function after the blanket bypass was removed.
///
/// This is intentionally broad for day-one migration — it preserves the
/// pre-ADR-0046 behavior of System principals being universally allowed,
/// but makes that authority an auditable, overridable Cedar policy rather
/// than hard-coded control flow. Follow-up work narrows this policy to the
/// specific actions the platform genuinely needs (bootstrap writes,
/// credential rotation, recovery).
const SYSTEM_PLATFORM_POLICY: &str = r#"
@id("system-platform:broad-permit")
permit(principal is System, action, resource);
"#;

/// PolicyId prefix used for the built-in system-platform policies
/// (ADR-0046). Used to exclude them from user-facing counts.
const SYSTEM_PLATFORM_POLICY_ID_PREFIX: &str = "system-platform:";

/// Merge the built-in system-platform policy into an existing [`PolicySet`].
///
/// Policies are added with explicit `PolicyId`s prefixed by
/// [`SYSTEM_PLATFORM_POLICY_ID_PREFIX`] so downstream code can filter them
/// out of user-facing reports (see [`count_user_policies`]). If the
/// hard-coded system policy fails to parse, the combined set is left
/// unchanged — preserving availability at the cost of System auth.
fn merge_system_platform_policy(combined: &mut PolicySet) {
    let system_set: PolicySet = match SYSTEM_PLATFORM_POLICY.parse() {
        Ok(ps) => ps,
        Err(_) => return,
    };
    for (idx, policy) in system_set.policies().enumerate() {
        let named = policy.clone().new_id(PolicyId::new(format!(
            "{SYSTEM_PLATFORM_POLICY_ID_PREFIX}broad-permit-{idx}"
        )));
        let _ = combined.add(named);
    }
}

/// Count user-authored policies in a [`PolicySet`], excluding the built-in
/// `system-platform` policies (ADR-0046). Tenants should reason about their
/// own policy surface without the platform's internals polluting the count.
fn count_user_policies(ps: &PolicySet) -> usize {
    ps.policies()
        .filter(|p| {
            !p.id()
                .to_string()
                .starts_with(SYSTEM_PLATFORM_POLICY_ID_PREFIX)
        })
        .count()
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

fn resource_id_from_attrs(attrs: &HashMap<String, serde_json::Value>) -> String {
    attrs
        .get("id")
        .or_else(|| attrs.get("Id"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string()
}
