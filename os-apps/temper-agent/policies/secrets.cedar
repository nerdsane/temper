// Secret management — Cedar Authorization Policies
//
// Governs the tenant secrets API (`/api/tenants/{tenant}/secrets/...`), which
// authorizes action "manage_secrets" on resource type "Secret". The resource ID
// is the secret key name, or "__keys__" for the collection (list) endpoint.
//
// SCOPE NOTE: secrets management is a platform/tenant-administration concern,
// not an agent-app concern. It lives here because a tenant's Cedar policy set is
// assembled from its installed apps, and temper-agent is the app that needs
// operator-managed secrets (anthropic_api_key, tensorlake_api_key, github_token)
// to reach its WASM integrations. If the platform grows a tenant-level policy
// seed, this file should move there rather than being duplicated per app.
//
// Reading a secret from inside a WASM guest is a DIFFERENT action
// ("access_secret", see agent.cedar) and is not granted here.

// --- Admins: full control over tenant secrets ---

permit(
  principal is Admin,
  action == Action::"manage_secrets",
  resource is Secret
);

// --- Operators, supervisors, and humans: manage tenant secrets ---
//
// "operator" is the agent_type of the credential bootstrapped from
// TEMPER_API_KEY (see temper-platform/src/bootstrap.rs:bootstrap_operator_credential),
// which is the identity a local or CI operator presents.
//
// The tenant boundary is enforced ahead of policy evaluation by
// require_tenant_match, so a principal can never reach another tenant's
// secrets through this rule.

permit(
  principal,
  action == Action::"manage_secrets",
  resource is Secret
) when {
  ["operator", "supervisor", "human"].contains(principal.agent_type)
};
