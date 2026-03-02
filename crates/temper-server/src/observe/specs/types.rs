use serde::Deserialize;

/// Request body for POST /api/specs/load-dir.
#[derive(Deserialize)]
pub(crate) struct LoadDirRequest {
    /// Tenant name to register specs under.
    pub(crate) tenant: String,
    /// Path to the specs directory containing model.csdl.xml and *.ioa.toml files.
    pub(crate) specs_dir: String,
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
