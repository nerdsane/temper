//! Public value types for deterministic scoped specification bundles.

const MAX_ERROR_MESSAGE_BYTES: usize = 1_024;

/// One named Cedar policy source included in immutable bundle identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyArtifactInput {
    /// Stable logical policy name.
    pub name: String,
    /// Cedar policy source. CRLF line endings are canonicalized to LF.
    pub source: String,
}

/// One named WASM module descriptor included in immutable bundle identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WasmArtifactInput {
    /// Stable logical module name.
    pub name: String,
    /// Immutable lowercase SHA-256 digest of module bytes.
    pub artifact_digest: String,
}

/// Optional pure migration module descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationArtifactInput {
    /// Stable logical module name.
    pub name: String,
    /// Immutable lowercase SHA-256 digest of module bytes.
    pub artifact_digest: String,
    /// Closed migration ABI version, currently `temper-schema-migration/v1`.
    pub abi_version: String,
}

/// Explicit positive verification and migration budgets bound into identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedBundleBudgets {
    /// Maximum deterministic verification steps.
    pub verification_steps: u64,
    /// Maximum migration fuel consumed for one entity.
    pub migration_fuel_per_entity: u64,
    /// Maximum migration linear-memory pages.
    pub migration_memory_pages: u32,
    /// Maximum migration input bytes for one entity.
    pub migration_input_bytes: u32,
    /// Maximum migration output bytes for one entity.
    pub migration_output_bytes: u32,
    /// Maximum entities transformed in one durable batch.
    pub migration_entities_per_batch: u32,
    /// Maximum total entities transformed by one job.
    pub migration_total_entities: u64,
    /// Maximum durable batches consumed by one migration job.
    pub migration_total_batches: u64,
    /// Maximum fenced worker claims across crash recovery.
    pub migration_attempts: u32,
}

impl Default for ScopedBundleBudgets {
    fn default() -> Self {
        Self {
            verification_steps: 100_000,
            migration_fuel_per_entity: 1_000_000,
            migration_memory_pages: 256,
            migration_input_bytes: 1_048_576,
            migration_output_bytes: 1_048_576,
            migration_entities_per_batch: 100,
            migration_total_entities: 1_000_000,
            migration_total_batches: 10_000,
            migration_attempts: 100,
        }
    }
}

/// One named IOA source submitted as part of a scoped bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IoaSourceInput {
    /// Fully qualified CSDL entity type, such as `Example.Task`.
    pub entity_type: String,
    /// IOA TOML source whose typed automaton name matches the short entity name.
    pub source: String,
}

/// Owned inputs to deterministic scoped-bundle compilation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedSpecBundleInput {
    /// Opaque tenant-local task scope identifier.
    pub scope_id: String,
    /// Optional immutable predecessor in lowercase `sha256:<hex>` form.
    pub predecessor_digest: Option<String>,
    /// Complete OData CSDL XML projection.
    pub csdl_xml: String,
    /// IOA specifications. Input enumeration does not affect bundle identity.
    pub ioa_sources: Vec<IoaSourceInput>,
    /// Named Cedar sources. Input enumeration does not affect identity.
    pub cedar_policies: Vec<PolicyArtifactInput>,
    /// Named immutable WASM descriptors.
    pub wasm_modules: Vec<WasmArtifactInput>,
    /// Optional pure migration module descriptor.
    pub migration: Option<MigrationArtifactInput>,
    /// Explicit verification and migration budgets.
    pub budgets: ScopedBundleBudgets,
}

/// One validated IOA specification in canonical TOML form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalIoaSpec {
    /// Fully qualified entity type.
    pub entity_type: String,
    /// Deterministically serialized TOML, suitable for typed re-parsing.
    pub canonical_source: String,
}

/// Immutable deterministic foundation for a task-scoped schema deployment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedSpecBundle {
    pub(super) scope_id: String,
    pub(super) predecessor_digest: Option<String>,
    pub(super) canonical_csdl: String,
    pub(super) ioa_specs: Vec<CanonicalIoaSpec>,
    pub(super) cedar_policies: Vec<PolicyArtifactInput>,
    pub(super) wasm_modules: Vec<WasmArtifactInput>,
    pub(super) migration: Option<MigrationArtifactInput>,
    pub(super) budgets: ScopedBundleBudgets,
    pub(super) digest: String,
}

impl ScopedSpecBundle {
    /// Opaque tenant-local task scope identifier.
    pub fn scope_id(&self) -> &str {
        &self.scope_id
    }
    /// Optional predecessor bundle digest.
    pub fn predecessor_digest(&self) -> Option<&str> {
        self.predecessor_digest.as_deref()
    }
    /// Canonical OData CSDL XML.
    pub fn canonical_csdl(&self) -> &str {
        &self.canonical_csdl
    }
    /// IOA specifications sorted by fully qualified entity type.
    pub fn ioa_specs(&self) -> &[CanonicalIoaSpec] {
        &self.ioa_specs
    }
    /// Cedar policies sorted by logical name with LF line endings.
    pub fn cedar_policies(&self) -> &[PolicyArtifactInput] {
        &self.cedar_policies
    }
    /// WASM module descriptors sorted by logical name.
    pub fn wasm_modules(&self) -> &[WasmArtifactInput] {
        &self.wasm_modules
    }
    /// Optional pure migration module descriptor.
    pub fn migration(&self) -> Option<&MigrationArtifactInput> {
        self.migration.as_ref()
    }
    /// Explicit verification and migration budgets.
    pub fn budgets(&self) -> &ScopedBundleBudgets {
        &self.budgets
    }
    /// Lowercase, domain-separated SHA-256 identity.
    pub fn digest(&self) -> &str {
        &self.digest
    }
}

/// Stable machine-readable failure class for bundle compilation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BundleErrorCode {
    InvalidBundle,
    InvalidScope,
    InvalidPredecessor,
    InvalidCsdl,
    InvalidIoa,
    InvalidArtifact,
    InvalidMigration,
    EntityNameMismatch,
    DuplicateSymbol,
    BudgetExceeded,
}

impl BundleErrorCode {
    /// Stable snake-case code used by future API adapters.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidBundle => "invalid_bundle",
            Self::InvalidScope => "invalid_scope",
            Self::InvalidPredecessor => "invalid_predecessor",
            Self::InvalidCsdl => "invalid_csdl",
            Self::InvalidIoa => "invalid_ioa",
            Self::InvalidArtifact => "invalid_artifact",
            Self::InvalidMigration => "invalid_migration",
            Self::EntityNameMismatch => "entity_name_mismatch",
            Self::DuplicateSymbol => "duplicate_symbol",
            Self::BudgetExceeded => "bundle_budget_exhausted",
        }
    }
}

/// A stable failure code plus bounded human-readable context.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}", code = .code.as_str())]
pub struct BundleError {
    code: BundleErrorCode,
    message: String,
}

impl BundleError {
    /// Stable machine-readable failure class.
    pub const fn code(&self) -> BundleErrorCode {
        self.code
    }

    pub(super) fn new(code: BundleErrorCode, message: impl Into<String>) -> Self {
        let mut message = message.into();
        if message.len() > MAX_ERROR_MESSAGE_BYTES {
            let mut end = MAX_ERROR_MESSAGE_BYTES;
            while !message.is_char_boundary(end) {
                end -= 1;
            }
            message.truncate(end);
        }
        debug_assert!(message.len() <= MAX_ERROR_MESSAGE_BYTES);
        Self { code, message }
    }
}
