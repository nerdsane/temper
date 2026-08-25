//! System tenant bootstrap.
//!
//! Loads the platform's own entity specs (Project, Tenant, CatalogEntry,
//! Collaborator, Version), runs the verification cascade, and registers
//! them as the `temper-system` tenant. This is dogfooding: the platform
//! manages itself using its own framework.

use std::collections::BTreeMap;

use temper_runtime::tenant::TenantId;
use temper_server::platform_store::{PlatformStore, SpecVerificationUpdate};
use temper_server::registry::{
    EntityLevelSummary, EntityVerificationResult, RegistryError, VerificationStatus,
};
use temper_spec::automaton;
use temper_spec::csdl::{CsdlDocument, emit_csdl_xml, parse_csdl};
use temper_store_turso::spec_content_hash;
use temper_verify::cascade::VerificationCascade;

use crate::state::PlatformState;

/// System tenant ID.
pub const SYSTEM_TENANT: &str = "temper-system";

// Embed system specs at compile time.
const PROJECT_IOA: &str = include_str!("specs/Project.ioa.toml");
const TENANT_IOA: &str = include_str!("specs/Tenant.ioa.toml");
const CATALOG_ENTRY_IOA: &str = include_str!("specs/CatalogEntry.ioa.toml");
const COLLABORATOR_IOA: &str = include_str!("specs/Collaborator.ioa.toml");
const VERSION_IOA: &str = include_str!("specs/Version.ioa.toml");
const OBSERVATION_IOA: &str = include_str!("specs/Observation.ioa.toml");
const PROBLEM_IOA: &str = include_str!("specs/Problem.ioa.toml");
const ANALYSIS_IOA: &str = include_str!("specs/Analysis.ioa.toml");
const EVOLUTION_DECISION_IOA: &str = include_str!("specs/EvolutionDecision.ioa.toml");
const INSIGHT_IOA: &str = include_str!("specs/Insight.ioa.toml");
const FEATURE_REQUEST_IOA: &str = include_str!("specs/FeatureRequest.ioa.toml");
const GOVERNANCE_DECISION_IOA: &str = include_str!("specs/GovernanceDecision.ioa.toml");
const HTTP_ENDPOINT_IOA: &str = include_str!("specs/HttpEndpoint.ioa.toml");
const SYSTEM_CSDL: &str = include_str!("specs/model.csdl.xml");

/// All system entity specs as (entity_type, ioa_source) pairs.
const SYSTEM_SPECS: &[(&str, &str)] = &[
    ("Project", PROJECT_IOA),
    ("Tenant", TENANT_IOA),
    ("CatalogEntry", CATALOG_ENTRY_IOA),
    ("Collaborator", COLLABORATOR_IOA),
    ("Version", VERSION_IOA),
    ("Observation", OBSERVATION_IOA),
    ("Problem", PROBLEM_IOA),
    ("Analysis", ANALYSIS_IOA),
    ("EvolutionDecision", EVOLUTION_DECISION_IOA),
    ("Insight", INSIGHT_IOA),
    ("FeatureRequest", FEATURE_REQUEST_IOA),
    ("GovernanceDecision", GOVERNANCE_DECISION_IOA),
    ("HttpEndpoint", HTTP_ENDPOINT_IOA),
];

// Embed agent specs at compile time.
const AGENT_IOA: &str = include_str!("specs/agent.ioa.toml");
const AGENT_TYPE_IOA: &str = include_str!("specs/agent_type.ioa.toml");
const PLAN_IOA: &str = include_str!("specs/plan.ioa.toml");
const TASK_IOA: &str = include_str!("specs/task.ioa.toml");
const TOOL_CALL_IOA: &str = include_str!("specs/tool_call.ioa.toml");
const SCHEDULE_IOA: &str = include_str!("specs/schedule.ioa.toml");
const POLICY_IOA: &str = include_str!("specs/policy.ioa.toml");
const AGENT_CREDENTIAL_IOA: &str = include_str!("specs/agent_credential.ioa.toml");
const TRUSTED_ISSUER_IOA: &str = include_str!("specs/trusted_issuer.ioa.toml");
const PRINCIPAL_GENERATION_IOA: &str = include_str!("specs/principal_generation.ioa.toml");
const AGENT_CSDL: &str = include_str!("specs/agent_model.csdl.xml");

/// Agent entity specs as (entity_type, ioa_source) pairs.
const AGENT_SPECS: &[(&str, &str)] = &[
    ("Agent", AGENT_IOA),
    ("AgentType", AGENT_TYPE_IOA),
    ("Plan", PLAN_IOA),
    ("Task", TASK_IOA),
    ("ToolCall", TOOL_CALL_IOA),
    ("Schedule", SCHEDULE_IOA),
    ("Policy", POLICY_IOA),
    ("AgentCredential", AGENT_CREDENTIAL_IOA),
    ("TrustedIssuer", TRUSTED_ISSUER_IOA),
    ("PrincipalGeneration", PRINCIPAL_GENERATION_IOA),
];

/// Verify, parse, and register a set of IOA specs under a tenant.
///
/// Uses content-hash gating: if a spec's SHA-256 hash matches a previously
/// verified entry in `verified_cache`, the verification cascade is skipped.
/// This prevents the expensive Z3 + Stateright + proptest cascade from
/// running on every boot (which caused OOM on Railway's 512 MB containers).
///
/// Returns a list of `(entity_type, content_hash)` for all bootstrapped specs
/// so the caller can persist them to the backing store.
///
/// Panics if any spec fails to parse or verify (fatal startup error).
pub(crate) struct BootstrapTenantSpecsOptions<'a> {
    pub(crate) merge: bool,
    pub(crate) label: &'a str,
    pub(crate) verified_cache: &'a BTreeMap<String, (String, bool)>,
    pub(crate) cross_invariants_source: Option<&'a str>,
    pub(crate) verification_mode: BootstrapSpecVerificationMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BootstrapSpecVerificationMode {
    /// Run the full parse + formal/property verification cascade for changed specs.
    FullCascade,
    /// Trust the already-built app bundle after parse/CSDL registration.
    ///
    /// Production OS-app startup must not block readiness on the expensive verifier.
    /// CI/offline gates own that deeper proof; startup owns loading known bundle content.
    TrustBundle,
}

pub(crate) fn bootstrap_tenant_specs(
    state: &PlatformState,
    tenant: &str,
    csdl_source: &str,
    specs: &[(&str, &str)],
    options: BootstrapTenantSpecsOptions<'_>,
) -> Vec<(String, String)> {
    bootstrap_tenant_specs_inner(state, tenant, csdl_source, specs, options)
}

fn bootstrap_tenant_specs_inner(
    state: &PlatformState,
    tenant: &str,
    csdl_source: &str,
    specs: &[(&str, &str)],
    options: BootstrapTenantSpecsOptions<'_>,
) -> Vec<(String, String)> {
    let BootstrapTenantSpecsOptions {
        merge,
        label,
        verified_cache,
        cross_invariants_source,
        verification_mode,
    } = options;

    tracing::info!(
        "Bootstrapping {label} specs for tenant '{tenant}' with {} entities",
        specs.len()
    );

    // Validate all specs parse.
    for (entity_type, ioa_source) in specs {
        automaton::parse_automaton(ioa_source)
            .unwrap_or_else(|e| panic!("{label} spec {entity_type} failed to parse: {e}"));
    }

    // Hash-gated verification: only run the cascade for specs whose
    // content has changed since the last successful verification.
    let mut spec_hashes = Vec::with_capacity(specs.len());
    for (entity_type, ioa_source) in specs {
        let hash = spec_content_hash(ioa_source);
        let already_verified = verified_cache
            .get(*entity_type)
            .is_some_and(|(cached_hash, verified)| *verified && cached_hash == &hash);

        if already_verified {
            tracing::info!(
                "Spec {entity_type} unchanged (hash={}…), skipping verification",
                &hash[..8]
            );
        } else if verification_mode == BootstrapSpecVerificationMode::TrustBundle {
            tracing::info!(
                "Spec {entity_type} changed (hash={}…), trusting prebuilt bundle at bootstrap",
                &hash[..8]
            );
        } else {
            tracing::info!(
                "Spec {entity_type} needs verification (hash={}…), running cascade",
                &hash[..8]
            );
            let cascade = VerificationCascade::from_ioa(ioa_source)
                .with_sim_seeds(3)
                .with_prop_test_cases(20);
            let result = cascade.run();
            assert!(
                result.all_passed,
                "{label} spec {entity_type} failed verification cascade"
            );
        }
        spec_hashes.push((entity_type.to_string(), hash));
    }

    // Parse CSDL schema.
    let csdl =
        parse_csdl(csdl_source).unwrap_or_else(|e| panic!("{label} CSDL failed to parse: {e}"));

    // Register tenant and mark specs as pre-verified.
    let tenant_id = TenantId::new(tenant);
    {
        let mut registry = state.registry.write().unwrap(); // ci-ok: infallible lock
        registry
            .try_register_tenant_with_reactions_and_constraints(
                tenant_id.clone(),
                csdl,
                csdl_source.to_string(),
                specs,
                Vec::new(),
                cross_invariants_source.map(str::to_string),
                merge,
            )
            .unwrap_or_else(|e| panic!("failed to register {label} specs for '{tenant}': {e}"));
        let now = temper_runtime::scheduler::sim_now().to_rfc3339();
        for (entity_type, _) in specs {
            registry.set_verification_status(
                &tenant_id,
                entity_type,
                VerificationStatus::Completed(EntityVerificationResult {
                    all_passed: true,
                    levels: vec![EntityLevelSummary {
                        level: "Bootstrap".to_string(),
                        passed: true,
                        summary: "Pre-verified at bootstrap".to_string(),
                        details: None,
                    }],
                    verified_at: now.clone(),
                }),
            );
        }
    }

    tracing::info!(
        "{label} specs bootstrapped for tenant '{tenant}': {:?}",
        specs.iter().map(|(t, _)| *t).collect::<Vec<_>>()
    );

    spec_hashes
}

/// Bootstrap the system tenant.
///
/// Validates, verifies, and registers all temper-system entity specs.
/// Returns `(entity_type, content_hash)` pairs for persistence.
/// Panics if system specs fail to parse or verify (fatal startup error).
pub fn bootstrap_system_tenant(
    state: &PlatformState,
    verified_cache: &BTreeMap<String, (String, bool)>,
) -> Vec<(String, String)> {
    bootstrap_tenant_specs(
        state,
        SYSTEM_TENANT,
        SYSTEM_CSDL,
        SYSTEM_SPECS,
        BootstrapTenantSpecsOptions {
            merge: false,
            label: "System",
            verified_cache,
            cross_invariants_source: None,
            verification_mode: BootstrapSpecVerificationMode::FullCascade,
        },
    )
}

/// Bootstrap agent entity specs (Agent, Plan, Task, ToolCall) for a tenant.
///
/// Parses and verifies the agent IOA specs, then registers them under the
/// given tenant. Returns `(entity_type, content_hash)` pairs for persistence.
/// Panics if agent specs fail to parse or verify.
pub fn bootstrap_agent_specs(
    state: &PlatformState,
    tenant: &str,
    merge: bool,
    verified_cache: &BTreeMap<String, (String, bool)>,
) -> Vec<(String, String)> {
    bootstrap_tenant_specs(
        state,
        tenant,
        AGENT_CSDL,
        AGENT_SPECS,
        BootstrapTenantSpecsOptions {
            merge,
            label: "Agent",
            verified_cache,
            cross_invariants_source: None,
            verification_mode: BootstrapSpecVerificationMode::FullCascade,
        },
    )
}

/// Platform IDENTITY specs — a strict subset of [`AGENT_SPECS`] that backs
/// kernel JWT verification (RFC-0002, ARN-255). Reuses the kernel's own,
/// authoritative IOA spec constants; there is no copied spec text.
const IDENTITY_SPECS: &[(&str, &str)] = &[
    ("TrustedIssuer", TRUSTED_ISSUER_IOA),
    ("PrincipalGeneration", PRINCIPAL_GENERATION_IOA),
];

/// Specs a tenant needs before an operator credential can be issued — the
/// `AgentType` it is typed against and the `AgentCredential` that stores the
/// key hash. Also a strict subset of [`AGENT_SPECS`], reusing kernel constants.
const OPERATOR_CREDENTIAL_SPECS: &[(&str, &str)] = &[
    ("AgentType", AGENT_TYPE_IOA),
    ("AgentCredential", AGENT_CREDENTIAL_IOA),
];

/// Derive a minimal CSDL slice for `keep_types` from the authoritative
/// [`AGENT_CSDL`].
///
/// The slice is *derived*, never hand-copied: it keeps only the requested
/// entity types, the bound actions whose binding parameter targets one of them,
/// and the entity sets backed by them. Unrelated enum types, terms, and
/// operation imports are dropped so the emitted slice is self-contained. This
/// keeps a single source of truth (`AGENT_CSDL`) instead of a drifting mirror.
///
/// Panics only if `AGENT_CSDL` fails to parse — a compile-time invariant of an
/// embedded, test-covered constant (see `test_agent_csdl_parses`).
fn agent_csdl_slice(keep_types: &[&str]) -> CsdlDocument {
    let mut doc =
        parse_csdl(AGENT_CSDL).unwrap_or_else(|e| panic!("Agent CSDL failed to parse: {e}"));
    let is_kept = |type_name: &str| {
        let bare = type_name.rsplit('.').next().unwrap_or(type_name);
        keep_types.contains(&bare)
    };
    let binds_kept = |params: &[temper_spec::csdl::Parameter]| {
        params
            .iter()
            .find(|p| p.name == "bindingParameter")
            .is_some_and(|p| is_kept(&p.type_name))
    };
    for schema in &mut doc.schemas {
        schema.entity_types.retain(|et| is_kept(&et.name));
        schema.actions.retain(|a| binds_kept(&a.parameters));
        schema.functions.retain(|f| binds_kept(&f.parameters));
        schema.enum_types.clear();
        schema.terms.clear();
        for container in &mut schema.entity_containers {
            container.entity_sets.retain(|es| is_kept(&es.entity_type));
            container.action_imports.clear();
            container.function_imports.clear();
        }
    }
    doc
}

/// Register a strict subset of the built-in agent specs for a tenant WITHOUT
/// the verification cascade.
///
/// Merges (`merge = true`) a CSDL slice derived from [`AGENT_CSDL`] plus the
/// given IOA specs into the tenant's existing registration, so it is purely
/// additive — it never wipes entities or schema owned by an OS app such as
/// `paw-agent`. The specs are marked verified without running the
/// Z3 + Stateright + proptest cascade, exactly as OS-app install marks its
/// bundled specs; that keeps startup within memory-constrained containers where
/// the full default agent-spec bootstrap is deliberately skipped. CI/offline
/// gates own the deeper formal proof of these pinned specs.
///
/// Returns an error if registration fails; callers decide whether that is fatal.
fn bootstrap_agent_subset_specs(
    state: &PlatformState,
    tenant: &str,
    specs: &[(&str, &str)],
) -> Result<(), RegistryError> {
    let keep: Vec<&str> = specs.iter().map(|(entity_type, _)| *entity_type).collect();
    let csdl_doc = agent_csdl_slice(&keep);
    let csdl_xml = emit_csdl_xml(&csdl_doc);

    let tenant_id = TenantId::new(tenant);
    let mut registry = state.registry.write().unwrap(); // ci-ok: infallible startup lock

    registry.try_register_tenant_with_reactions_and_constraints(
        tenant_id.clone(),
        csdl_doc,
        csdl_xml,
        specs,
        Vec::new(),
        None,
        true, // merge: additive; preserve any OS-app entities and schema
    )?;

    // Trust-register: mark verified without the cascade, exactly as OS-app
    // install does for its bundled specs.
    let verified_at = temper_runtime::scheduler::sim_now().to_rfc3339();
    for (entity_type, _) in specs {
        registry.set_verification_status(
            &tenant_id,
            entity_type,
            VerificationStatus::Completed(EntityVerificationResult {
                all_passed: true,
                levels: vec![EntityLevelSummary {
                    level: "Bootstrap".to_string(),
                    passed: true,
                    summary: "Pre-verified platform subset spec at bootstrap".to_string(),
                    details: None,
                }],
                verified_at: verified_at.clone(),
            }),
        );
    }

    Ok(())
}

/// Register ONLY the platform IDENTITY specs (`TrustedIssuer`,
/// `PrincipalGeneration`) for a tenant, cascade-free.
///
/// This is the narrow public API a host uses when the default all-or-nothing
/// [`bootstrap_agent_specs`] is skipped (e.g. an OS app owns the agent
/// lifecycle) but the identity entities that back kernel JWT verification must
/// still be registered — without the host mirroring the kernel's spec text.
/// See [`bootstrap_agent_subset_specs`] for the merge / trust-bundle semantics.
///
/// Returns an error if registration fails so the caller can fail readiness
/// rather than silently leaving kernel JWT verification inert.
pub async fn bootstrap_identity_specs(
    state: &PlatformState,
    tenant: &str,
) -> Result<(), RegistryError> {
    bootstrap_agent_subset_specs(state, tenant, IDENTITY_SPECS)
}

/// Register the `AgentType` + `AgentCredential` specs a tenant needs before an
/// operator credential can be bootstrapped, cascade-free.
///
/// On a virgin store an OS app such as `paw-agent` may not own these two specs,
/// so [`bootstrap_operator_credential`] would otherwise dispatch against missing
/// tables and fail. A host calls this first so the credential bootstrap resolves
/// against a registered entity set on the very first boot (ARN-255 cold-boot).
/// Same merge / trust-bundle semantics as [`bootstrap_identity_specs`].
pub async fn bootstrap_operator_credential_specs(
    state: &PlatformState,
    tenant: &str,
) -> Result<(), RegistryError> {
    bootstrap_agent_subset_specs(state, tenant, OPERATOR_CREDENTIAL_SPECS)
}

/// Persist built-in spec hashes and verification status to Turso.
///
/// After bootstrap verifies specs (or skips via cache), this writes each
/// spec into the `specs` table with its content hash and marks it verified.
/// On subsequent boots, `load_verification_cache` finds these rows and
/// the cascade is skipped — preventing OOM on memory-constrained hosts.
///
/// Note: the upsert + mark-verified is two statements, not atomic. If the
/// process crashes between them the spec row will have `verified=0` until
/// we recommit the tenant's spec set at the end — safe, just slower.
pub(crate) async fn persist_bootstrap_verification(
    store: &dyn PlatformStore,
    tenant: &str,
    specs: &[(&str, &str)],
    csdl_source: &str,
    hashes: &[(String, String)],
    verified_cache: &BTreeMap<String, (String, bool)>,
) {
    let hashes_to_persist = hashes_requiring_persistence(hashes, verified_cache);
    let mut wrote_specs = false;

    for (entity_type, content_hash) in &hashes_to_persist {
        // Find the IOA source for this entity type.
        let ioa_source = specs
            .iter()
            .find(|(et, _)| *et == entity_type)
            .map(|(_, src)| *src)
            .expect("hash returned for unknown entity type");

        // Upsert the spec row (preserves verification if hash unchanged).
        if let Err(e) = store
            .upsert_spec(tenant, entity_type, ioa_source, csdl_source, content_hash)
            .await
        {
            tracing::warn!("Failed to persist bootstrap spec {tenant}/{entity_type}: {e}");
            continue;
        }
        wrote_specs = true;

        // Mark as verified (bootstrap panics on failure, so all specs here passed).
        if let Err(e) = store
            .persist_spec_verification(
                tenant,
                entity_type,
                SpecVerificationUpdate {
                    status: "completed",
                    verified: true,
                    levels_passed: None,
                    levels_total: None,
                    verification_result_json: None,
                },
            )
            .await
        {
            tracing::warn!("Failed to persist verification status for {tenant}/{entity_type}: {e}");
        }
    }

    // `upsert_spec` marks rows as uncommitted while content is rewritten. Once
    // bootstrap verification succeeds, promote the tenant's spec set back to a
    // durable committed state so restart recovery can actually see the rows.
    if wrote_specs && let Err(e) = store.commit_specs(tenant).await {
        tracing::warn!("Failed to commit bootstrap specs for tenant '{tenant}': {e}");
    }
}

fn hashes_requiring_persistence(
    hashes: &[(String, String)],
    verified_cache: &BTreeMap<String, (String, bool)>,
) -> Vec<(String, String)> {
    hashes
        .iter()
        .filter(|(entity_type, content_hash)| {
            !verified_cache
                .get(entity_type)
                .is_some_and(|(cached_hash, verified)| *verified && cached_hash == content_hash)
        })
        .cloned()
        .collect()
}

/// Persist system tenant spec verification to the platform store.
pub async fn persist_system_verification(
    store: &dyn PlatformStore,
    hashes: &[(String, String)],
    verified_cache: &BTreeMap<String, (String, bool)>,
) {
    persist_bootstrap_verification(
        store,
        SYSTEM_TENANT,
        SYSTEM_SPECS,
        SYSTEM_CSDL,
        hashes,
        verified_cache,
    )
    .await;
}

/// Persist agent spec verification to the platform store.
pub async fn persist_agent_verification(
    store: &dyn PlatformStore,
    tenant: &str,
    hashes: &[(String, String)],
    verified_cache: &BTreeMap<String, (String, bool)>,
) {
    persist_bootstrap_verification(
        store,
        tenant,
        AGENT_SPECS,
        AGENT_CSDL,
        hashes,
        verified_cache,
    )
    .await;
}

/// Register the bootstrap key as a normal operator credential in one tenant.
///
/// When the platform boots with a `TEMPER_API_KEY` configured, this function
/// ensures a corresponding `AgentType` ("operator") and `AgentCredential`
/// exist in the given tenant so the bearer auth middleware can resolve the
/// key as a verified identity in that tenant. Registration in one tenant
/// deliberately grants no authority in any other tenant.
///
/// Also seeds a narrow Cedar permit so a verified operator can
/// `manage_policies` on that tenant's `PolicySet` (ADR-0172). The permit is
/// merged into live Cedar, persisted as a granular row, and is idempotent.
///
/// This is idempotent: if the entities and permit already exist (e.g., from a
/// previous boot), the actions are no-ops.
///
/// Returns an error if either dispatch (defining the operator `AgentType` or
/// issuing its `AgentCredential`) fails — for example on a virgin store where
/// the `AgentType` / `AgentCredential` entity sets were never registered. The
/// caller is expected to surface that loudly (and, in production, fail
/// readiness) rather than boot with an operator key that resolves to a 401.
/// Callers that need the entity sets guaranteed should call
/// [`bootstrap_operator_credential_specs`] first.
pub async fn bootstrap_operator_credential(
    state: &PlatformState,
    api_key: &str,
    tenant: &str,
) -> Result<(), String> {
    use temper_server::identity::hash_token;

    let tenant_id = temper_runtime::tenant::TenantId::new(tenant);
    let agent_ctx = temper_server::request_context::AgentContext::for_service("platform-bootstrap");
    let agent_type_id = "operator-type";
    let instance_id = "operator";

    // Step 1: Ensure AgentType "operator-type" exists and is Active.
    // Create the entity first (starts in Draft state).
    state
        .server
        .dispatch_tenant_action(
            &tenant_id,
            "AgentType",
            agent_type_id,
            "Define",
            serde_json::json!({
                "name": "operator",
                "system_prompt": "Tenant-scoped platform operator",
                "tool_set": "local",
                "model": "none",
                "max_turns": "0",
                "adapter_config": "{}",
                "default_budget_cents": "0"
            }),
            &agent_ctx,
        )
        .await
        .map_err(|e| format!("failed to define operator AgentType for tenant '{tenant}': {e}"))?;

    // Step 2: Create and issue AgentCredential for the API key hash.
    let key_hash = hash_token(api_key);
    let key_prefix = api_key.chars().take(8).collect::<String>();

    state
        .server
        .dispatch_tenant_action(
            &tenant_id,
            "AgentCredential",
            &key_hash,
            "Issue",
            serde_json::json!({
                "agent_type_id": agent_type_id,
                "agent_instance_id": instance_id,
                "key_hash": key_hash,
                "key_prefix": key_prefix,
                "description": "Tenant-scoped operator bootstrap credential",
                "created_by": "bootstrap",
                "expires_at": ""
            }),
            &agent_ctx,
        )
        .await
        .map_err(|e| {
            format!("failed to issue operator AgentCredential for tenant '{tenant}': {e}")
        })?;

    crate::operator_manage_policies::seed_operator_manage_policies(state, tenant).await;

    tracing::info!(
        "Operator credential bootstrapped for tenant '{tenant}' (key_hash={}...)",
        &key_hash[..8]
    );
    Ok(())
}

/// Outcome of [`bootstrap_trusted_issuer_from_env`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssuerBootstrapOutcome {
    /// No issuer env vars were set — nothing to register (safe no-op).
    Skipped,
    /// The trusted issuer was registered from the environment.
    Registered,
}

/// Classify the three trusted-issuer env values into an action, without touching
/// the environment (pure, so it is directly testable).
///
/// - `Ok(None)` — none of the three are set: a safe no-op.
/// - `Ok(Some((url, jwks, audience)))` — all three are set: register.
/// - `Err(_)` — some but not all are set: a misconfiguration. Registering a
///   partial config would leave JWT verification quietly inert, so this is an
///   error the caller can fail readiness on rather than a silent no-op.
fn resolve_issuer_env_config(
    url: Option<String>,
    jwks: Option<String>,
    audience: Option<String>,
) -> Result<Option<(String, String, String)>, String> {
    match (url, jwks, audience) {
        (Some(u), Some(j), Some(a)) => Ok(Some((u, j, a))),
        (None, None, None) => Ok(None),
        (u, j, a) => {
            let mut missing = Vec::new();
            if u.is_none() {
                missing.push("TEMPER_TRUSTED_ISSUER_URL");
            }
            if j.is_none() {
                missing.push("TEMPER_TRUSTED_ISSUER_JWKS");
            }
            if a.is_none() {
                missing.push("TEMPER_TRUSTED_ISSUER_AUD");
            }
            Err(format!(
                "trusted-issuer env is partially configured; missing: {}",
                missing.join(", ")
            ))
        }
    }
}

/// Register a trusted JWT issuer from environment configuration at startup.
///
/// Mirrors [`bootstrap_operator_credential`]: a deployment activates the
/// platform-issued-token path (ARN-255) by setting three env vars — the issuer
/// URL, its inline JWKS, and the expected audience — instead of making an
/// authenticated API call. Registration goes through the normal dispatch path
/// under a service context, so it is not gated on a tenant Cedar policy.
///
/// Returns:
/// - `Ok(Skipped)` when none of the three env vars are set (safe no-op).
/// - `Ok(Registered)` when the issuer was registered (idempotent: re-registering
///   an existing issuer is a self-loop on `Active`).
/// - `Err(_)` when the config is *partial* (some but not all three vars set) or
///   registration fails. A partial config is a misconfiguration, not a no-op —
///   registering half a trusted-issuer config would leave JWT verification
///   quietly inert — so the caller can fail readiness rather than log a
///   misleading success.
pub async fn bootstrap_trusted_issuer_from_env(
    state: &PlatformState,
    tenant: &str,
) -> Result<IssuerBootstrapOutcome, String> {
    let url = std::env::var("TEMPER_TRUSTED_ISSUER_URL")
        .ok()
        .filter(|s| !s.is_empty());
    let jwks = std::env::var("TEMPER_TRUSTED_ISSUER_JWKS")
        .ok()
        .filter(|s| !s.is_empty());
    let aud = std::env::var("TEMPER_TRUSTED_ISSUER_AUD")
        .ok()
        .filter(|s| !s.is_empty());

    let (issuer, jwks_json, audience) = match resolve_issuer_env_config(url, jwks, aud)
        .map_err(|e| format!("{e} (tenant '{tenant}')"))?
    {
        Some(config) => config,
        None => return Ok(IssuerBootstrapOutcome::Skipped),
    };

    let tenant_id = temper_runtime::tenant::TenantId::new(tenant);
    // Registering a trusted issuer is System-only (system-platform Cedar policy);
    // the platform seeding itself acts as System.
    let agent_ctx = temper_server::request_context::AgentContext::system();
    let algorithms =
        std::env::var("TEMPER_TRUSTED_ISSUER_ALGS").unwrap_or_else(|_| "ES256".to_string());

    state
        .server
        .dispatch_tenant_action(
            &tenant_id,
            "TrustedIssuer",
            &issuer,
            "RegisterIssuer",
            serde_json::json!({
                "issuer": issuer,
                "jwks_json": jwks_json,
                "audience": audience,
                "algorithms": algorithms,
                "description": "Registered from environment at startup",
                "created_by": "bootstrap",
            }),
            &agent_ctx,
        )
        .await
        .map_err(|e| {
            format!("failed to register trusted issuer '{issuer}' for tenant '{tenant}': {e}")
        })?;

    tracing::info!("Trusted issuer '{issuer}' registered for tenant '{tenant}' from environment");
    Ok(IssuerBootstrapOutcome::Registered)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_system_specs_parse() {
        for (entity_type, ioa_source) in SYSTEM_SPECS {
            let result = automaton::parse_automaton(ioa_source);
            assert!(
                result.is_ok(),
                "System spec {} failed to parse: {:?}",
                entity_type,
                result.err()
            );
        }
    }

    #[test]
    fn test_system_csdl_parses() {
        let result = parse_csdl(SYSTEM_CSDL);
        assert!(
            result.is_ok(),
            "System CSDL failed to parse: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_bootstrap_registers_system_tenant() {
        let state = PlatformState::new(None);

        bootstrap_system_tenant(&state, &BTreeMap::new());

        let registry = state.registry.read().unwrap();
        let tenant = TenantId::new(SYSTEM_TENANT);

        assert!(registry.get_tenant(&tenant).is_some());
        assert!(registry.get_table(&tenant, "Project").is_some());
        assert!(registry.get_table(&tenant, "Tenant").is_some());
        assert!(registry.get_table(&tenant, "CatalogEntry").is_some());
        assert!(registry.get_table(&tenant, "Collaborator").is_some());
        assert!(registry.get_table(&tenant, "Version").is_some());
    }

    #[test]
    fn test_system_spec_entity_names() {
        for (entity_type, ioa_source) in SYSTEM_SPECS {
            let automaton = automaton::parse_automaton(ioa_source).unwrap();
            assert_eq!(
                automaton.automaton.name, *entity_type,
                "Spec name mismatch: expected {entity_type}, got {}",
                automaton.automaton.name
            );
        }
    }

    #[test]
    fn test_system_specs_verify() {
        for (entity_type, ioa_source) in SYSTEM_SPECS {
            let cascade = VerificationCascade::from_ioa(ioa_source)
                .with_sim_seeds(3)
                .with_prop_test_cases(50);
            let result = cascade.run();
            assert!(
                result.all_passed,
                "System spec {} failed verification",
                entity_type
            );
        }
    }

    #[test]
    fn test_project_initial_state() {
        let automaton = automaton::parse_automaton(PROJECT_IOA).unwrap();
        assert_eq!(automaton.automaton.initial, "Created");
        assert_eq!(automaton.automaton.states.len(), 4);
    }

    #[test]
    fn test_tenant_initial_state() {
        let automaton = automaton::parse_automaton(TENANT_IOA).unwrap();
        assert_eq!(automaton.automaton.initial, "Pending");
        assert_eq!(automaton.automaton.states.len(), 5);
    }

    #[test]
    fn test_entity_types_count() {
        assert_eq!(SYSTEM_SPECS.len(), 13);
    }

    #[test]
    fn test_observation_initial_state() {
        let automaton = automaton::parse_automaton(OBSERVATION_IOA).unwrap();
        assert_eq!(automaton.automaton.initial, "Open");
        assert_eq!(automaton.automaton.states.len(), 4);
    }

    #[test]
    fn test_problem_initial_state() {
        let automaton = automaton::parse_automaton(PROBLEM_IOA).unwrap();
        assert_eq!(automaton.automaton.initial, "Open");
        assert_eq!(automaton.automaton.states.len(), 4);
    }

    #[test]
    fn test_analysis_initial_state() {
        let automaton = automaton::parse_automaton(ANALYSIS_IOA).unwrap();
        assert_eq!(automaton.automaton.initial, "Open");
        assert_eq!(automaton.automaton.states.len(), 4);
    }

    #[test]
    fn test_evolution_decision_initial_state() {
        let automaton = automaton::parse_automaton(EVOLUTION_DECISION_IOA).unwrap();
        assert_eq!(automaton.automaton.initial, "Open");
        assert_eq!(automaton.automaton.states.len(), 4);
    }

    #[test]
    fn test_insight_initial_state() {
        let automaton = automaton::parse_automaton(INSIGHT_IOA).unwrap();
        assert_eq!(automaton.automaton.initial, "Open");
        assert_eq!(automaton.automaton.states.len(), 4);
    }

    #[test]
    fn test_feature_request_initial_state() {
        let automaton = automaton::parse_automaton(FEATURE_REQUEST_IOA).unwrap();
        assert_eq!(automaton.automaton.initial, "Open");
        assert_eq!(automaton.automaton.states.len(), 5);
    }

    #[test]
    fn test_governance_decision_initial_state() {
        let automaton = automaton::parse_automaton(GOVERNANCE_DECISION_IOA).unwrap();
        assert_eq!(automaton.automaton.initial, "Pending");
        assert_eq!(automaton.automaton.states.len(), 4);
    }

    #[test]
    fn test_http_endpoint_initial_state() {
        let automaton = automaton::parse_automaton(HTTP_ENDPOINT_IOA).unwrap();
        assert_eq!(automaton.automaton.initial, "Active");
        assert_eq!(automaton.automaton.states.len(), 3);
    }

    #[test]
    fn test_bootstrap_registers_new_entities() {
        let state = PlatformState::new(None);

        bootstrap_system_tenant(&state, &BTreeMap::new());

        let registry = state.registry.read().unwrap();
        let tenant = TenantId::new(SYSTEM_TENANT);

        assert!(registry.get_table(&tenant, "Observation").is_some());
        assert!(registry.get_table(&tenant, "Problem").is_some());
        assert!(registry.get_table(&tenant, "Analysis").is_some());
        assert!(registry.get_table(&tenant, "EvolutionDecision").is_some());
        assert!(registry.get_table(&tenant, "Insight").is_some());
        assert!(registry.get_table(&tenant, "FeatureRequest").is_some());
        assert!(registry.get_table(&tenant, "GovernanceDecision").is_some());
        assert!(registry.get_table(&tenant, "HttpEndpoint").is_some());
    }

    // ── Agent Spec Tests ────────────────────────────────────────────

    #[test]
    fn test_agent_specs_parse() {
        for (entity_type, ioa_source) in AGENT_SPECS {
            let result = automaton::parse_automaton(ioa_source);
            assert!(
                result.is_ok(),
                "Agent spec {} failed to parse: {:?}",
                entity_type,
                result.err()
            );
        }
    }

    #[test]
    fn test_agent_csdl_parses() {
        let result = parse_csdl(AGENT_CSDL);
        assert!(
            result.is_ok(),
            "Agent CSDL failed to parse: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_agent_spec_entity_names() {
        for (entity_type, ioa_source) in AGENT_SPECS {
            let automaton = automaton::parse_automaton(ioa_source).unwrap();
            assert_eq!(
                automaton.automaton.name, *entity_type,
                "Agent spec name mismatch: expected {entity_type}, got {}",
                automaton.automaton.name
            );
        }
    }

    #[test]
    fn test_agent_specs_verify() {
        for (entity_type, ioa_source) in AGENT_SPECS {
            let cascade = VerificationCascade::from_ioa(ioa_source)
                .with_sim_seeds(3)
                .with_prop_test_cases(50);
            let result = cascade.run();
            assert!(
                result.all_passed,
                "Agent spec {} failed verification",
                entity_type
            );
        }
    }

    #[test]
    fn test_bootstrap_agent_specs_registers_tenant() {
        let state = PlatformState::new(None);
        bootstrap_agent_specs(&state, "test-agent", false, &BTreeMap::new());
        let registry = state.registry.read().unwrap();
        let tenant = TenantId::new("test-agent");
        assert!(registry.get_tenant(&tenant).is_some());
        assert!(registry.get_table(&tenant, "Agent").is_some());
        assert!(registry.get_table(&tenant, "AgentType").is_some());
        assert!(registry.get_table(&tenant, "Plan").is_some());
        assert!(registry.get_table(&tenant, "Task").is_some());
        assert!(registry.get_table(&tenant, "ToolCall").is_some());
    }

    #[test]
    fn test_bootstrap_agent_specs_merge_preserves_existing_app_entity_sets() {
        let state = PlatformState::new(None);
        let tenant = TenantId::new("app-tenant");
        let custom_csdl = r#"<?xml version="1.0" encoding="UTF-8"?>
<edmx:Edmx Version="4.0"
  xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Example"
      xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Widget">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.Guid" Nullable="false"/>
      </EntityType>
      <EntityContainer Name="ExampleService">
        <EntitySet Name="Widgets" EntityType="Temper.Example.Widget"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;
        let custom_ioa = r#"
[automaton]
name = "Widget"
states = ["Created"]
initial = "Created"
"#;

        {
            let mut registry = state.registry.write().unwrap();
            registry.register_tenant(
                tenant.clone(),
                parse_csdl(custom_csdl).unwrap(),
                custom_csdl.to_string(),
                &[("Widget", custom_ioa)],
            );
        }

        bootstrap_agent_specs(&state, "app-tenant", true, &BTreeMap::new());

        let registry = state.registry.read().unwrap();
        assert!(
            registry.get_table(&tenant, "Widget").is_some(),
            "custom app entity should survive merged agent bootstrap"
        );
        assert!(
            registry.get_table(&tenant, "Agent").is_some(),
            "agent entities should be added during merged bootstrap"
        );
        assert_eq!(
            registry.resolve_entity_type(&tenant, "Widgets").as_deref(),
            Some("Widget"),
            "existing app entity-set mapping should survive merged bootstrap"
        );
    }

    #[test]
    fn test_hashes_requiring_persistence_skip_cached_verified_specs() {
        let hashes = vec![
            ("Agent".to_string(), "sha256:agent".to_string()),
            ("Plan".to_string(), "sha256:plan".to_string()),
            ("Task".to_string(), "sha256:task".to_string()),
        ];
        let mut verified_cache = BTreeMap::new();
        verified_cache.insert("Agent".to_string(), ("sha256:agent".to_string(), true));
        verified_cache.insert("Plan".to_string(), ("sha256:plan".to_string(), false));

        let pending = hashes_requiring_persistence(&hashes, &verified_cache);

        assert_eq!(
            pending,
            vec![
                ("Plan".to_string(), "sha256:plan".to_string()),
                ("Task".to_string(), "sha256:task".to_string()),
            ]
        );
    }

    #[test]
    fn test_agent_specs_count() {
        assert_eq!(AGENT_SPECS.len(), 10);
    }

    #[test]
    fn test_trusted_issuer_spec_is_registered() {
        assert!(
            AGENT_SPECS
                .iter()
                .any(|(name, source)| *name == "TrustedIssuer" && !source.is_empty())
        );
    }

    #[test]
    fn test_principal_generation_spec_is_registered() {
        assert!(
            AGENT_SPECS
                .iter()
                .any(|(name, source)| *name == "PrincipalGeneration" && !source.is_empty())
        );
    }

    // ── Identity / subset bootstrap (ARN-255) ───────────────────────

    #[test]
    fn identity_specs_are_a_strict_subset_of_agent_specs() {
        for (name, source) in IDENTITY_SPECS {
            assert!(
                AGENT_SPECS
                    .iter()
                    .any(|(n, s)| n == name && s == source && !s.is_empty()),
                "identity spec {name} must reuse the AGENT_SPECS constant verbatim"
            );
        }
        for (name, source) in OPERATOR_CREDENTIAL_SPECS {
            assert!(
                AGENT_SPECS
                    .iter()
                    .any(|(n, s)| n == name && s == source && !s.is_empty()),
                "operator-credential spec {name} must reuse the AGENT_SPECS constant verbatim"
            );
        }
    }

    #[test]
    fn agent_csdl_slice_keeps_only_requested_identity_entities() {
        let doc = agent_csdl_slice(&["TrustedIssuer", "PrincipalGeneration"]);
        let schema = doc
            .schemas
            .iter()
            .find(|s| s.namespace == "Temper")
            .expect("Temper namespace present");

        let mut type_names: Vec<&str> = schema
            .entity_types
            .iter()
            .map(|e| e.name.as_str())
            .collect();
        type_names.sort_unstable();
        assert_eq!(type_names, vec!["PrincipalGeneration", "TrustedIssuer"]);

        let sets: Vec<&str> = schema.entity_containers[0]
            .entity_sets
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert!(sets.contains(&"TrustedIssuers"));
        assert!(sets.contains(&"PrincipalGenerations"));
        assert_eq!(sets.len(), 2, "no non-identity entity sets should leak in");

        // Bound actions for the identity entities are preserved; nothing bound
        // to a non-identity entity leaks through.
        assert!(schema.actions.iter().any(|a| a.name == "RegisterIssuer"));
        assert!(schema.actions.iter().any(|a| a.name == "BumpGeneration"));
        for action in &schema.actions {
            let binds = action
                .parameters
                .iter()
                .find(|p| p.name == "bindingParameter")
                .map(|p| p.type_name.clone())
                .unwrap_or_default();
            assert!(
                binds.ends_with("TrustedIssuer") || binds.ends_with("PrincipalGeneration"),
                "action {} unexpectedly bound to {binds}",
                action.name
            );
        }

        // The derived slice must round-trip through the XML emitter/parser.
        let xml = emit_csdl_xml(&doc);
        parse_csdl(&xml).expect("emitted identity slice must re-parse");
    }

    #[tokio::test]
    async fn bootstrap_identity_specs_registers_both_entity_sets() {
        let state = PlatformState::new(None);
        bootstrap_identity_specs(&state, "app-tenant")
            .await
            .expect("identity specs should register");

        let registry = state.registry.read().unwrap();
        let tenant = TenantId::new("app-tenant");
        assert!(
            registry.get_table(&tenant, "TrustedIssuer").is_some(),
            "TrustedIssuer entity should be registered"
        );
        assert!(
            registry.get_table(&tenant, "PrincipalGeneration").is_some(),
            "PrincipalGeneration entity should be registered"
        );
        assert_eq!(
            registry
                .resolve_entity_type(&tenant, "TrustedIssuers")
                .as_deref(),
            Some("TrustedIssuer"),
            "TrustedIssuers set should resolve to the TrustedIssuer type"
        );
    }

    #[tokio::test]
    async fn bootstrap_identity_specs_merge_preserves_existing_app_entities() {
        // The identity registration must be additive: an OS app's already
        // registered entities and schema survive it (merge = true).
        let state = PlatformState::new(None);
        let tenant = TenantId::new("app-tenant");
        let custom_csdl = r#"<?xml version="1.0" encoding="UTF-8"?>
<edmx:Edmx Version="4.0"
  xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Example"
      xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Widget">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.Guid" Nullable="false"/>
      </EntityType>
      <EntityContainer Name="ExampleService">
        <EntitySet Name="Widgets" EntityType="Temper.Example.Widget"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;
        let custom_ioa =
            "[automaton]\nname = \"Widget\"\nstates = [\"Created\"]\ninitial = \"Created\"\n";
        {
            let mut registry = state.registry.write().unwrap();
            registry.register_tenant(
                tenant.clone(),
                parse_csdl(custom_csdl).unwrap(),
                custom_csdl.to_string(),
                &[("Widget", custom_ioa)],
            );
        }

        bootstrap_identity_specs(&state, "app-tenant")
            .await
            .expect("identity specs should register");

        let registry = state.registry.read().unwrap();
        assert!(
            registry.get_table(&tenant, "Widget").is_some(),
            "existing app entity should survive identity bootstrap"
        );
        assert!(
            registry.get_table(&tenant, "TrustedIssuer").is_some(),
            "identity entity should be added by the merge"
        );
        assert_eq!(
            registry.resolve_entity_type(&tenant, "Widgets").as_deref(),
            Some("Widget"),
            "existing app entity-set mapping should survive identity bootstrap"
        );
    }

    #[tokio::test]
    async fn bootstrap_operator_credential_specs_registers_agent_type_and_credential() {
        let state = PlatformState::new(None);
        bootstrap_operator_credential_specs(&state, "cold-tenant")
            .await
            .expect("operator credential specs should register");

        let registry = state.registry.read().unwrap();
        let tenant = TenantId::new("cold-tenant");
        assert!(
            registry.get_table(&tenant, "AgentType").is_some(),
            "AgentType should be registered before operator credential bootstrap"
        );
        assert!(
            registry.get_table(&tenant, "AgentCredential").is_some(),
            "AgentCredential should be registered before operator credential bootstrap"
        );
    }

    #[tokio::test]
    async fn bootstrap_operator_credential_specs_then_credential_resolves_cold_boot() {
        // ARN-255 cold-boot: on a virgin store where no OS app owns AgentType /
        // AgentCredential, registering the subset specs first lets the operator
        // credential bootstrap succeed on the FIRST boot instead of erroring.
        let state = PlatformState::new(None);
        bootstrap_operator_credential_specs(&state, "cold-tenant")
            .await
            .expect("subset specs register");
        bootstrap_operator_credential(&state, "tmpr_cold-boot-key", "cold-tenant")
            .await
            .expect("operator credential bootstrap should succeed after subset specs");
    }

    #[tokio::test]
    async fn bootstrap_operator_credential_errors_without_specs() {
        // Without the AgentType/AgentCredential specs the dispatch has no entity
        // set to target — the bootstrap must surface an error, not silently
        // no-op into a later 401.
        let state = PlatformState::new(None);
        let result =
            bootstrap_operator_credential(&state, "tmpr_no-specs-key", "virgin-tenant").await;
        assert!(
            result.is_err(),
            "operator credential bootstrap must error when specs are unregistered"
        );
    }

    #[test]
    fn issuer_env_config_none_set_is_skip() {
        assert_eq!(resolve_issuer_env_config(None, None, None), Ok(None));
    }

    #[test]
    fn issuer_env_config_all_set_is_register() {
        assert_eq!(
            resolve_issuer_env_config(
                Some("https://issuer.example".into()),
                Some("{\"keys\":[]}".into()),
                Some("temper".into()),
            ),
            Ok(Some((
                "https://issuer.example".into(),
                "{\"keys\":[]}".into(),
                "temper".into(),
            )))
        );
    }

    #[test]
    fn issuer_env_config_partial_is_error_naming_missing_vars() {
        // URL set, JWKS + AUD missing — a misconfiguration, not a no-op.
        let err = resolve_issuer_env_config(Some("https://issuer.example".into()), None, None)
            .expect_err("partial issuer config must be an error");
        assert!(err.contains("TEMPER_TRUSTED_ISSUER_JWKS"));
        assert!(err.contains("TEMPER_TRUSTED_ISSUER_AUD"));
        assert!(!err.contains("TEMPER_TRUSTED_ISSUER_URL"));

        // The inverse partial (only AUD missing) is likewise an error.
        let err2 = resolve_issuer_env_config(
            Some("https://issuer.example".into()),
            Some("{\"keys\":[]}".into()),
            None,
        )
        .expect_err("partial issuer config must be an error");
        assert!(err2.contains("TEMPER_TRUSTED_ISSUER_AUD"));
    }
}
