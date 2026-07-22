use serde::Deserialize;

/// Request body for POST /api/specs/load-dir.
#[derive(Deserialize)]
pub(crate) struct LoadDirRequest {
    /// Tenant name to register specs under.
    pub(crate) tenant: String,
    /// Path to the specs directory containing model.csdl.xml and *.ioa.toml files.
    pub(crate) specs_dir: String,
    /// When `true`, merge incoming specs with existing tenant config instead of
    /// replacing. Used by `load-inline` so that agent-submitted specs don't
    /// wipe platform entity types.
    #[serde(default)]
    pub(crate) merge: bool,
    /// Optional Cedar policy text merged into the tenant's complete policy and
    /// committed/activated in the same guarded runtime generation.
    #[serde(default)]
    pub(crate) cedar_policies: Option<String>,
}

/// Request body for POST /api/specs/load-inline.
#[derive(Deserialize)]
pub(crate) struct LoadInlineRequest {
    /// Tenant name to register specs under.
    pub(crate) tenant: String,
    /// Optional app slug for ADR lookup, e.g. `llm-wiki`.
    #[serde(default)]
    pub(crate) app_name: Option<String>,
    /// Map of filename -> content. Must include `model.csdl.xml` and at least one `*.ioa.toml`.
    pub(crate) specs: std::collections::BTreeMap<String, String>,
    /// Optional inline `cross-invariants.toml` source.
    #[serde(default)]
    pub(crate) cross_invariants_toml: Option<String>,
    /// Optional Cedar policy text to bundle with the spec deployment.
    #[serde(default)]
    pub(crate) cedar_policies: Option<String>,
}

/// Request body for POST /api/specs/validate-ioa.
#[derive(Deserialize)]
pub(crate) struct ValidateIoaRequest {
    /// IOA TOML source to validate with the server's current verification engine.
    pub(crate) ioa_source: String,
    /// Optional simulation seed budget. Defaults to the server quick-check budget.
    #[serde(default)]
    pub(crate) sim_seeds: Option<u64>,
    /// Optional property-test case budget. Defaults to the server quick-check budget.
    #[serde(default)]
    pub(crate) prop_test_cases: Option<u32>,
}
