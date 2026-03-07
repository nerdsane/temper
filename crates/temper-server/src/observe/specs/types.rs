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
}

/// Request body for POST /api/specs/load-inline.
#[derive(Deserialize)]
pub(crate) struct LoadInlineRequest {
    /// Tenant name to register specs under.
    pub(crate) tenant: String,
    /// Map of filename -> content. Must include `model.csdl.xml` and at least one `*.ioa.toml`.
    pub(crate) specs: std::collections::BTreeMap<String, String>,
    /// Optional inline `cross-invariants.toml` source.
    #[serde(default)]
    pub(crate) cross_invariants_toml: Option<String>,
}
