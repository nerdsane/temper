// ============================================================
// Temper Platform Preset Policies (Tier 1 — Non-negotiable)
// ============================================================
//
// These policies are always loaded alongside user policies.
// They use `forbid` to override any user `permit` rules.
// Cannot be removed or overridden by developers.

// Deny all WASM modules from calling localhost / loopback / metadata endpoints.
// Prevents SSRF attacks against internal infrastructure.
forbid(
    principal is Agent,
    action == Action::"http_call",
    resource is HttpEndpoint
) when {
    context has "module" &&
    (resource.domain == "localhost" ||
     resource.domain == "127.0.0.1" ||
     resource.domain == "0.0.0.0" ||
     resource.domain == "metadata.google.internal" ||
     resource.domain == "169.254.169.254")
};

// Deny access to the vault master key.
// No WASM module should ever read the encryption key.
forbid(
    principal is Agent,
    action == Action::"access_secret",
    resource == Secret::"TEMPER_VAULT_KEY"
) when {
    context has "module"
};

// Deny access to internal Temper configuration secrets.
forbid(
    principal is Agent,
    action == Action::"access_secret",
    resource == Secret::"TEMPER_DATABASE_URL"
) when {
    context has "module"
};

// ============================================================
// Dispatch Presets: Require specs for consequential actions
// ============================================================

// Deployment actions require a spec.
@id("require-spec-for-deploy")
forbid(
    principal,
    action == Action::"deploy",
    resource
) when {
    context has "has_spec" && context.has_spec == false
};

// Actions with WASM integrations require a spec.
@id("require-spec-for-wasm-execute")
forbid(
    principal,
    action == Action::"execute",
    resource
) when {
    context has "has_wasm_integration" &&
    context.has_wasm_integration == true &&
    context has "has_spec" &&
    context.has_spec == false
};

// ============================================================
// Spec Quality Presets: Structural invariants on submitted specs
// ============================================================

// Require guard on transitions to states containing "deploy" or "production".
@id("require-guard-on-deploy-states")
forbid(
    principal,
    action == Action::"submit_specs",
    resource
) when {
    context has "states" &&
    context has "guarded_target_states" &&
    (context.states.contains("Deployed") || context.states.contains("Production")) &&
    !(context.guarded_target_states.contains("Deployed") || context.guarded_target_states.contains("Production"))
};

// Require at least one invariant for specs with more than 3 states.
@id("require-invariants-for-complex-specs")
forbid(
    principal,
    action == Action::"submit_specs",
    resource
) when {
    context has "state_count" &&
    context has "invariant_count" &&
    context.state_count > 3 &&
    context.invariant_count == 0
};
