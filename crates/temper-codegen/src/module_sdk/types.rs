use temper_wasm_sdk::data::ModuleSdkManifest;

/// Generated Rust source and the activation manifest that binds it.
#[derive(Debug, Clone)]
pub struct GeneratedModuleSdk {
    /// Complete generated Rust guest source.
    pub source: String,
    /// Canonical activation binding packaged beside the compiled artifact.
    pub manifest: ModuleSdkManifest,
}

/// Compiled WASM bytes with an artifact-carried SDK binding and matching sidecar.
#[derive(Debug, Clone)]
pub struct PackagedModuleSdk {
    /// WebAssembly bytes containing the host-readable custom section.
    pub wasm: Vec<u8>,
    /// Sidecar manifest whose artifact digest covers the exact packaged bytes.
    pub manifest: ModuleSdkManifest,
}
