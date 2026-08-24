//! Canonical module capability and SDK binding metadata.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{DATA_ABI_VERSION_V1, ModuleSdkCompatibilityProof};

mod operation;

pub use operation::DataOperationKind;

/// Per-module application-data capability declaration.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModuleDataGrant {
    /// Closed operation kinds granted to the module.
    #[serde(default)]
    pub operations: BTreeSet<DataOperationKind>,
    /// Exact entity and action surface granted to the module.
    #[serde(default)]
    pub entities: Vec<EntityDataGrant>,
    /// Explicit invocation budgets.
    #[serde(default)]
    pub budgets: ModuleDataBudgets,
}

impl ModuleDataGrant {
    /// Validate syntax-level invariants that do not require schema metadata.
    pub fn validate(&self) -> Result<(), String> {
        self.budgets.validate()?;
        let mut entity_types = BTreeSet::new();
        let mut runtime_entity_types = BTreeSet::new();
        for entity in &self.entities {
            if entity.entity_type.trim().is_empty() {
                return Err("module data entity type must not be empty".into());
            }
            if !entity_types.insert(entity.entity_type.as_str()) {
                return Err(format!(
                    "duplicate module data entity grant '{}'",
                    entity.entity_type
                ));
            }
            let runtime_name = entity
                .entity_type
                .rsplit('.')
                .next()
                .unwrap_or(entity.entity_type.as_str());
            if !runtime_entity_types.insert(runtime_name) {
                return Err(format!(
                    "module data entity grants are runtime-ambiguous for short type '{runtime_name}'"
                ));
            }
            entity.validate()?;
        }
        Ok(())
    }

    /// Whether the grant permits an operation for an exact entity type.
    pub fn permits(
        &self,
        operation: DataOperationKind,
        entity_type: &str,
        action: Option<&str>,
    ) -> bool {
        if !self.operations.contains(&operation) {
            return false;
        }
        let Some(entity) = self
            .entities
            .iter()
            .find(|entity| entity.entity_type == entity_type)
        else {
            return false;
        };
        match operation {
            DataOperationKind::SchemaBundleSubmit
            | DataOperationKind::SchemaBundleGet
            | DataOperationKind::SchemaBundleVerify
            | DataOperationKind::SchemaBundleActivate
            | DataOperationKind::SchemaBundleRetire
            | DataOperationKind::SchemaMigrationStart
            | DataOperationKind::SchemaMigrationGet
            | DataOperationKind::SchemaMigrationRetry => true,
            DataOperationKind::ActionInvoke => {
                action.is_some_and(|name| entity.actions.contains(name))
            }
            DataOperationKind::CompositeInvoke => {
                action.is_some_and(|name| entity.composite_actions.contains(name))
            }
            _ => true,
        }
    }

    /// Stable SHA-256 digest of the canonical serialized grant.
    pub fn digest(&self) -> Result<String, String> {
        let mut canonical = self.clone();
        canonical
            .entities
            .sort_by(|left, right| left.entity_type.cmp(&right.entity_type));
        let bytes = serde_json::to_vec(&canonical)
            .map_err(|error| format!("failed to serialize module data grant: {error}"))?;
        Ok(hex_sha256(&bytes))
    }
}

/// Exact per-entity capability surface.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntityDataGrant {
    /// Fully qualified CSDL entity type.
    #[serde(rename = "type")]
    pub entity_type: String,
    /// Bound actions available to generated code.
    #[serde(default)]
    pub actions: BTreeSet<String>,
    /// Verified composite actions available to generated code.
    #[serde(default)]
    pub composite_actions: BTreeSet<String>,
    /// Fields accepted in v1 query filters.
    #[serde(default)]
    pub query_filter_fields: BTreeSet<String>,
    /// Fields accepted in v1 query ordering.
    #[serde(default)]
    pub query_order_fields: BTreeSet<String>,
    /// Whether generated queries may order by the host-owned commit sequence.
    #[serde(default, skip_serializing_if = "is_false")]
    pub query_order_by_sequence: bool,
    /// File operations available for this File entity type.
    #[serde(default)]
    pub file_operations: BTreeSet<FileOperationKind>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl EntityDataGrant {
    fn validate(&self) -> Result<(), String> {
        for value in self
            .actions
            .iter()
            .chain(self.composite_actions.iter())
            .chain(self.query_filter_fields.iter())
            .chain(self.query_order_fields.iter())
        {
            if value.trim().is_empty() {
                return Err(format!(
                    "module data grant '{}' contains an empty schema name",
                    self.entity_type
                ));
            }
        }
        Ok(())
    }
}

/// File-specific capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileOperationKind {
    MetadataRead,
    VersionRead,
    ContentRead,
    ContentWrite,
}

/// Explicit positive budgets enforced by generated clients and the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModuleDataBudgets {
    /// Maximum application-data calls during one module invocation.
    pub max_calls: u32,
    /// Maximum items in one non-atomic batch.
    pub max_batch_items: u32,
    /// Maximum values requested in one query page.
    pub max_page_items: u32,
    /// Maximum encoded request size.
    pub max_request_bytes: u32,
    /// Maximum encoded response size.
    pub max_response_bytes: u32,
    /// Maximum response handles held by the guest at once.
    pub max_open_responses: u32,
    /// Maximum File stream handles held by the guest at once.
    pub max_open_streams: u32,
    /// Maximum bytes transferred by one File stream.
    pub max_stream_bytes: u64,
}

impl ModuleDataBudgets {
    /// Smallest response budget that can hold a compact committed acknowledgement.
    pub const MIN_RESPONSE_BYTES: u32 = 4_096;
    /// Platform ceiling used to reject unreasonable module declarations.
    pub const PLATFORM_MAX: Self = Self {
        max_calls: 1_024,
        max_batch_items: 1_024,
        max_page_items: 1_000,
        max_request_bytes: 4 * 1024 * 1024,
        max_response_bytes: 16 * 1024 * 1024,
        max_open_responses: 64,
        max_open_streams: 32,
        max_stream_bytes: 1024 * 1024 * 1024,
    };

    /// Validate that every budget is positive and under the platform ceiling.
    pub fn validate(&self) -> Result<(), String> {
        let pairs = [
            (
                "max_calls",
                self.max_calls as u64,
                Self::PLATFORM_MAX.max_calls as u64,
            ),
            (
                "max_batch_items",
                self.max_batch_items as u64,
                Self::PLATFORM_MAX.max_batch_items as u64,
            ),
            (
                "max_page_items",
                self.max_page_items as u64,
                Self::PLATFORM_MAX.max_page_items as u64,
            ),
            (
                "max_request_bytes",
                self.max_request_bytes as u64,
                Self::PLATFORM_MAX.max_request_bytes as u64,
            ),
            (
                "max_response_bytes",
                self.max_response_bytes as u64,
                Self::PLATFORM_MAX.max_response_bytes as u64,
            ),
            (
                "max_open_responses",
                self.max_open_responses as u64,
                Self::PLATFORM_MAX.max_open_responses as u64,
            ),
            (
                "max_open_streams",
                self.max_open_streams as u64,
                Self::PLATFORM_MAX.max_open_streams as u64,
            ),
            (
                "max_stream_bytes",
                self.max_stream_bytes,
                Self::PLATFORM_MAX.max_stream_bytes,
            ),
        ];
        for (name, value, ceiling) in pairs {
            if value == 0 || value > ceiling {
                return Err(format!(
                    "module data budget {name} must be between 1 and {ceiling}, got {value}"
                ));
            }
        }
        if self.max_response_bytes < Self::MIN_RESPONSE_BYTES {
            return Err(format!(
                "module data budget max_response_bytes must be at least {}",
                Self::MIN_RESPONSE_BYTES
            ));
        }
        Ok(())
    }
}

impl Default for ModuleDataBudgets {
    fn default() -> Self {
        Self {
            max_calls: 32,
            max_batch_items: 64,
            max_page_items: 100,
            max_request_bytes: 1024 * 1024,
            max_response_bytes: 4 * 1024 * 1024,
            max_open_responses: 4,
            max_open_streams: 4,
            max_stream_bytes: 64 * 1024 * 1024,
        }
    }
}

/// Canonical schema symbol included in a generated SDK manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestEntityV1 {
    /// Fully qualified canonical CSDL entity type.
    pub entity_type: String,
    /// Canonical entity-set name.
    pub entity_set: String,
    /// Generated Rust type name.
    pub generated_name: String,
    /// Canonical property metadata in deterministic order.
    #[serde(default)]
    pub properties: Vec<ManifestPropertyV1>,
    /// Bound action metadata in deterministic order.
    #[serde(default)]
    pub actions: Vec<ManifestActionV1>,
}

/// Canonical property metadata used for typed generation and host validation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestPropertyV1 {
    /// Case-sensitive CSDL property or parameter name.
    pub canonical_name: String,
    /// Generated Rust field name.
    pub generated_name: String,
    /// Fully qualified CSDL scalar, enum, or reference type.
    pub type_name: String,
    /// Whether the canonical value may be null.
    pub nullable: bool,
    /// Closed enum members, empty for non-enum properties.
    #[serde(default)]
    pub enum_members: Vec<String>,
}

/// Canonical action metadata used for typed generation and host validation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestActionV1 {
    /// Case-sensitive IOA/CSDL action name.
    pub canonical_name: String,
    /// Generated Rust method name.
    pub generated_name: String,
    /// Non-binding action parameters.
    #[serde(default)]
    pub parameters: Vec<ManifestPropertyV1>,
    /// Canonical result type when the action returns a value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_type: Option<String>,
    /// Closed enum members for an enum result type.
    #[serde(default)]
    pub result_enum_members: Vec<String>,
    /// Whether this action uses the verified composite-action path.
    pub composite: bool,
}

/// Deterministically ordered binding packaged with a compiled module.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModuleSdkManifest {
    /// Version of the application-data host ABI.
    pub abi: u32,
    /// Exact app-manifest module name.
    pub module_name: String,
    /// Digest of the immutable resolved application closure.
    pub closure_digest: String,
    /// Independently recomputable dependency lock digest.
    pub dependency_lock_digest: String,
    /// Digest of canonical CSDL generation input.
    pub schema_digest: String,
    /// Digest of the schema symbols emitted into this SDK.
    pub used_symbols_digest: String,
    /// Version of the SDK generator.
    pub generator_version: String,
    /// Digest of the canonical least-privilege grant.
    pub grant_digest: String,
    /// SHA-256 digest of the compiled WASM artifact.
    pub artifact_digest: String,
    /// Exact runtime capability grant.
    pub grant: ModuleDataGrant,
    /// Canonical metadata for generated entities.
    #[serde(default)]
    pub entities: Vec<ManifestEntityV1>,
    /// Exact canonical schema symbols used by generated source.
    #[serde(default)]
    pub used_symbols: BTreeSet<String>,
    /// Optional host-verifiable proof for a pinned additive closure change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compatibility_proof: Option<ModuleSdkCompatibilityProof>,
}

/// Canonical immutable metadata digests consumed by SDK generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleSdkMetadataDigests {
    /// Digest of the resolved schema closure.
    pub closure: String,
    /// Digest of the independently resolved dependency lock.
    pub dependency_lock: String,
    /// Digest of canonical CSDL input.
    pub schema: String,
}

impl ModuleSdkManifest {
    /// Construct a manifest after computing the canonical grant digest.
    pub fn new(
        module_name: impl Into<String>,
        metadata: ModuleSdkMetadataDigests,
        artifact_digest: impl Into<String>,
        grant: ModuleDataGrant,
        entities: Vec<ManifestEntityV1>,
        used_symbols: BTreeSet<String>,
    ) -> Result<Self, String> {
        grant.validate()?;
        let mut grant = grant;
        grant
            .entities
            .sort_by(|left, right| left.entity_type.cmp(&right.entity_type));
        let grant_digest = grant.digest()?;
        let used_symbols_digest = digest_json(&used_symbols)?;
        Ok(Self {
            abi: DATA_ABI_VERSION_V1,
            module_name: module_name.into(),
            dependency_lock_digest: metadata.dependency_lock,
            closure_digest: metadata.closure,
            schema_digest: metadata.schema,
            used_symbols_digest,
            generator_version: env!("CARGO_PKG_VERSION").into(),
            grant_digest,
            artifact_digest: artifact_digest.into(),
            grant,
            entities,
            used_symbols,
            compatibility_proof: None,
        })
    }

    /// Stable digest of every activation-relevant binding field.
    pub fn binding_digest(&self) -> Result<String, String> {
        let bytes = serde_json::to_vec(self)
            .map_err(|error| format!("failed to serialize module SDK manifest: {error}"))?;
        Ok(hex_sha256(&bytes))
    }

    /// Recompute and verify the embedded grant digest and ABI version.
    pub fn verify_binding(&self) -> Result<(), String> {
        if self.abi != DATA_ABI_VERSION_V1 {
            return Err(format!("unsupported module data ABI {}", self.abi));
        }
        self.grant.validate()?;
        let actual = self.grant.digest()?;
        if actual != self.grant_digest {
            return Err("module data grant digest mismatch".into());
        }
        if self.dependency_lock_digest != self.closure_digest {
            return Err("module dependency lock and closure digests differ".into());
        }
        if digest_json(&self.used_symbols)? != self.used_symbols_digest {
            return Err("module used-symbol digest mismatch".into());
        }
        if self.generator_version != env!("CARGO_PKG_VERSION") {
            return Err("module SDK generator version mismatch".into());
        }
        Ok(())
    }

    /// Canonical semantic hash for every generated entity, property, and action.
    pub fn used_symbol_hashes(&self) -> Result<BTreeMap<String, String>, String> {
        let mut hashes = BTreeMap::new();
        for entity in &self.entities {
            hashes.insert(
                format!("entity:{}", entity.entity_type),
                digest_json(&(
                    &entity.entity_type,
                    &entity.entity_set,
                    &entity.generated_name,
                ))?,
            );
            hashes.insert(
                format!("entity_set:{}", entity.entity_set),
                digest_json(&entity.entity_set)?,
            );
            for property in &entity.properties {
                hashes.insert(
                    format!(
                        "property:{}:{}",
                        entity.entity_type, property.canonical_name
                    ),
                    digest_json(property)?,
                );
            }
            for action in &entity.actions {
                hashes.insert(
                    format!("action:{}:{}", entity.entity_type, action.canonical_name),
                    digest_json(action)?,
                );
            }
        }
        Ok(hashes)
    }
}

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn digest_json(value: &impl Serialize) -> Result<String, String> {
    serde_json::to_vec(value)
        .map(|bytes| hex_sha256(&bytes))
        .map_err(|error| format!("failed to serialize canonical manifest value: {error}"))
}

#[cfg(test)]
mod tests;
