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
