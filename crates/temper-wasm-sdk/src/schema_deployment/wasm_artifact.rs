//! Immutable WASM artifact transport binding.

use serde::{Deserialize, Serialize};

/// One immutable WASM module binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchemaWasmArtifactV1 {
    pub name: String,
    pub artifact_digest: String,
    /// Optional generated-client binding carried by this exact scoped module.
    ///
    /// Scope, tenant, and principal identity are deliberately absent. The host
    /// binds this manifest to the invocation's immutable schema pin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_binding: Option<crate::data::ModuleSdkManifest>,
}
