//! Host-readable module SDK binding stored in a WebAssembly custom section.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{ModuleSdkCompatibilityProof, ModuleSdkManifest};

const WASM_MAGIC_AND_VERSION: &[u8; 8] = b"\0asm\x01\0\0\0";
const BINDING_SECTION_NAME: &str = "temper.module_sdk_binding.v1";

/// Binding fields physically carried by the exact loaded WebAssembly artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactModuleSdkBinding {
    /// Version of the application-data host ABI.
    pub abi: u32,
    /// Exact app-manifest module name.
    pub module_name: String,
    /// Digest of the immutable dependency closure used for compilation.
    pub closure_digest: String,
    /// Independently resolved immutable dependency-lock digest.
    pub dependency_lock_digest: String,
    /// Digest of canonical schema generation input.
    pub schema_digest: String,
    /// Digest of the generated symbol set.
    pub used_symbols_digest: String,
    /// Canonical per-symbol semantic hashes.
    pub used_symbol_hashes: BTreeMap<String, String>,
    /// SDK generator version.
    pub generator_version: String,
    /// Exact capability grant digest.
    pub grant_digest: String,
    /// Optional compatibility evidence covered by this artifact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compatibility_proof: Option<ModuleSdkCompatibilityProof>,
}

impl ArtifactModuleSdkBinding {
    /// Derive artifact metadata from a verified manifest.
    pub fn from_manifest(manifest: &ModuleSdkManifest) -> Result<Self, String> {
        Ok(Self {
            abi: manifest.abi,
            module_name: manifest.module_name.clone(),
            closure_digest: manifest.closure_digest.clone(),
            dependency_lock_digest: manifest.dependency_lock_digest.clone(),
            schema_digest: manifest.schema_digest.clone(),
            used_symbols_digest: manifest.used_symbols_digest.clone(),
            used_symbol_hashes: manifest.used_symbol_hashes()?,
            generator_version: manifest.generator_version.clone(),
            grant_digest: manifest.grant_digest.clone(),
            compatibility_proof: manifest.compatibility_proof.clone(),
        })
    }
}

/// Append the canonical binding custom section to an unbound WebAssembly module.
pub fn bind_module_sdk_artifact(
    wasm: &[u8],
    binding: &ArtifactModuleSdkBinding,
) -> Result<Vec<u8>, String> {
    validate_wasm_header(wasm)?;
    if read_module_sdk_artifact_binding(wasm)?.is_some() {
        return Err("module SDK binding section already exists".into());
    }
    let binding_bytes = serde_json::to_vec(binding)
        .map_err(|error| format!("failed to serialize module SDK binding: {error}"))?;
    let mut payload = Vec::new();
    encode_uleb(BINDING_SECTION_NAME.len(), &mut payload)?;
    payload.extend_from_slice(BINDING_SECTION_NAME.as_bytes());
    payload.extend_from_slice(&binding_bytes);

    let mut packaged = Vec::with_capacity(wasm.len().saturating_add(payload.len() + 8));
    packaged.extend_from_slice(wasm);
    packaged.push(0);
    encode_uleb(payload.len(), &mut packaged)?;
    packaged.extend_from_slice(&payload);
    Ok(packaged)
}

/// Read and decode the unique module SDK binding custom section.
pub fn read_module_sdk_artifact_binding(
    wasm: &[u8],
) -> Result<Option<ArtifactModuleSdkBinding>, String> {
    validate_wasm_header(wasm)?;
    let mut offset = WASM_MAGIC_AND_VERSION.len();
    let mut found = None;
    while offset < wasm.len() {
        let section_id = *wasm
            .get(offset)
            .ok_or_else(|| "truncated WebAssembly section id".to_string())?;
        offset = offset.saturating_add(1);
        let section_len = decode_uleb(wasm, &mut offset)?;
        let section_end = offset
            .checked_add(section_len)
            .filter(|end| *end <= wasm.len())
            .ok_or_else(|| "WebAssembly section exceeds artifact bytes".to_string())?;
        if section_id == 0 {
            let mut cursor = offset;
            let name_len = decode_uleb(wasm, &mut cursor)?;
            let name_end = cursor
                .checked_add(name_len)
                .filter(|end| *end <= section_end)
                .ok_or_else(|| "WebAssembly custom-section name is truncated".to_string())?;
            let name = std::str::from_utf8(&wasm[cursor..name_end])
                .map_err(|_| "WebAssembly custom-section name is not UTF-8".to_string())?;
            if name == BINDING_SECTION_NAME {
                if found.is_some() {
                    return Err("multiple module SDK binding sections".into());
                }
                found = Some(
                    serde_json::from_slice(&wasm[name_end..section_end])
                        .map_err(|error| format!("invalid module SDK binding section: {error}"))?,
                );
            }
        }
        offset = section_end;
    }
    Ok(found)
}

fn validate_wasm_header(wasm: &[u8]) -> Result<(), String> {
    if !wasm.starts_with(WASM_MAGIC_AND_VERSION) {
        return Err("module artifact is not WebAssembly version 1".into());
    }
    Ok(())
}

fn encode_uleb(value: usize, output: &mut Vec<u8>) -> Result<(), String> {
    let mut value = u64::try_from(value).map_err(|_| "WebAssembly section is too large")?;
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        output.push(byte);
        if value == 0 {
            return Ok(());
        }
    }
}

fn decode_uleb(bytes: &[u8], offset: &mut usize) -> Result<usize, String> {
    let mut value = 0_u64;
    for shift in (0..=28).step_by(7) {
        let byte = *bytes
            .get(*offset)
            .ok_or_else(|| "truncated WebAssembly section length".to_string())?;
        *offset = offset.saturating_add(1);
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return usize::try_from(value)
                .map_err(|_| "WebAssembly section length exceeds platform size".into());
        }
    }
    Err("WebAssembly section length is not canonical u32 LEB128".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_binding_round_trips_and_rejects_duplicates() {
        let binding = ArtifactModuleSdkBinding {
            abi: 1,
            module_name: "worker".into(),
            closure_digest: "closure".into(),
            dependency_lock_digest: "closure".into(),
            schema_digest: "schema".into(),
            used_symbols_digest: "symbols".into(),
            used_symbol_hashes: BTreeMap::new(),
            generator_version: "1".into(),
            grant_digest: "grant".into(),
            compatibility_proof: None,
        };
        let wasm = bind_module_sdk_artifact(WASM_MAGIC_AND_VERSION, &binding).unwrap();
        assert_eq!(
            read_module_sdk_artifact_binding(&wasm).unwrap(),
            Some(binding.clone())
        );
        assert!(bind_module_sdk_artifact(&wasm, &binding).is_err());
    }
}
