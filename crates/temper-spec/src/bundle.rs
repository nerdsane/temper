//! Pure, deterministic compilation of immutable scoped specification bundles.

use std::collections::{BTreeMap, BTreeSet};

use crate::automaton::{
    Automaton, BundleLintFinding, LintSeverity, lint_automata_bundle, lint_automata_csdl_bundle,
    lint_automaton, lint_csdl_reference_contracts, parse_automaton,
};
use crate::csdl::parse_csdl;

mod csdl;
mod digest;
mod types;

use csdl::canonical_csdl;
use digest::{bundle_digest, module_data_closure_digest};
pub use types::{
    BundleError, BundleErrorCode, CanonicalIoaSpec, IoaSourceInput, MigrationArtifactInput,
    PolicyArtifactInput, ScopedBundleBudgets, ScopedSpecBundle, ScopedSpecBundleInput,
    WasmArtifactInput,
};

/// Canonicalization and digest contract implemented by this compiler.
pub const SCOPED_SPEC_BUNDLE_CONTRACT_V1: &str = "scoped-spec-bundle/v1";

const MAX_SCOPE_ID_BYTES: usize = 256;
const MAX_ENTITY_TYPE_BYTES: usize = 512;
const MAX_IOA_SPECS: usize = 1_024;
const MAX_IOA_SOURCE_BYTES: usize = 1_048_576;
const MAX_CSDL_BYTES: usize = 16_777_216;
const MAX_NAMED_ARTIFACTS: usize = 1_024;
const MAX_ARTIFACT_NAME_BYTES: usize = 256;
const MAX_POLICY_SOURCE_BYTES: usize = 1_048_576;
impl ScopedSpecBundle {
    /// Parse, validate, canonicalize, and digest one immutable scoped bundle.
    pub fn compile(input: ScopedSpecBundleInput) -> Result<Self, BundleError> {
        validate_scope(&input.scope_id)?;
        validate_predecessor(input.predecessor_digest.as_deref())?;
        if input.csdl_xml.len() > MAX_CSDL_BYTES {
            return Err(BundleError::new(
                BundleErrorCode::BudgetExceeded,
                format!("CSDL exceeds v1 byte budget {MAX_CSDL_BYTES}"),
            ));
        }
        if input.ioa_sources.is_empty() {
            return Err(BundleError::new(
                BundleErrorCode::InvalidIoa,
                "a scoped bundle must contain at least one IOA specification",
            ));
        }
        if input.ioa_sources.len() > MAX_IOA_SPECS {
            return Err(BundleError::new(
                BundleErrorCode::BudgetExceeded,
                format!(
                    "IOA specification count {} exceeds v1 budget {MAX_IOA_SPECS}",
                    input.ioa_sources.len()
                ),
            ));
        }

        let canonical_csdl = canonical_csdl(&input.csdl_xml)?;
        let ioa_specs = canonical_ioa_specs(input.ioa_sources)?;
        validate_bundle_contracts(&canonical_csdl, &ioa_specs)?;
        let cedar_policies = canonical_policies(input.cedar_policies)?;
        let wasm_modules = canonical_wasm_modules(input.wasm_modules)?;
        let migration = validate_migration(input.migration)?;
        validate_budgets(&input.budgets)?;
        let digest = bundle_digest(
            &input.scope_id,
            input.predecessor_digest.as_deref(),
            &canonical_csdl,
            &ioa_specs,
            &cedar_policies,
            &wasm_modules,
            migration.as_ref(),
            &input.budgets,
        );

        Ok(Self {
            scope_id: input.scope_id,
            predecessor_digest: input.predecessor_digest,
            canonical_csdl,
            ioa_specs,
            cedar_policies,
            wasm_modules,
            migration,
            budgets: input.budgets,
            digest,
        })
    }
}

/// Compute the immutable generated-client closure for scoped CSDL and IOA inputs.
///
/// Module artifacts and the enclosing scoped bundle are deliberately excluded
/// so guest compilation cannot create a digest cycle.
pub fn scoped_module_data_closure_digest(
    csdl_xml: &str,
    ioa_sources: Vec<IoaSourceInput>,
) -> Result<String, BundleError> {
    if csdl_xml.len() > MAX_CSDL_BYTES {
        return Err(BundleError::new(
            BundleErrorCode::BudgetExceeded,
            format!("CSDL exceeds v1 byte budget {MAX_CSDL_BYTES}"),
        ));
    }
    let canonical_csdl = canonical_csdl(csdl_xml)?;
    let ioa_specs = canonical_ioa_specs(ioa_sources)?;
    validate_bundle_contracts(&canonical_csdl, &ioa_specs)?;
    Ok(module_data_closure_digest(&canonical_csdl, &ioa_specs))
}

fn validate_bundle_contracts(
    canonical_csdl: &str,
    ioa_specs: &[CanonicalIoaSpec],
) -> Result<(), BundleError> {
    let csdl = parse_csdl(canonical_csdl).map_err(|error| {
        BundleError::new(
            BundleErrorCode::InvalidCsdl,
            format!("failed to reparse canonical CSDL: {error}"),
        )
    })?;
    let csdl_entities = csdl
        .schemas
        .iter()
        .flat_map(|schema| {
            schema
                .entity_types
                .iter()
                .map(move |entity| format!("{}.{}", schema.namespace, entity.name))
        })
        .collect::<BTreeSet<_>>();
    let mut automata = BTreeMap::<String, Automaton>::new();
    for spec in ioa_specs {
        if !csdl_entities.contains(&spec.entity_type) {
            return Err(BundleError::new(
                BundleErrorCode::InvalidBundle,
                format!(
                    "IOA entity '{}' is absent from the canonical CSDL",
                    spec.entity_type
                ),
            ));
        }
        let automaton = parse_automaton(&spec.canonical_source).map_err(|error| {
            BundleError::new(
                BundleErrorCode::InvalidIoa,
                format!(
                    "failed to reparse canonical IOA '{}': {error}",
                    spec.entity_type
                ),
            )
        })?;
        let short_name = automaton.automaton.name.clone();
        if automata.insert(short_name.clone(), automaton).is_some() {
            return Err(BundleError::new(
                BundleErrorCode::InvalidBundle,
                format!("IOA short name '{short_name}' is ambiguous across CSDL namespaces"),
            ));
        }
    }

    let stream_capabilities = crate::csdl::verify_stream_capabilities_v1(&csdl)
        .map_err(|error| BundleError::new(BundleErrorCode::InvalidBundle, error.to_string()))?;
    crate::csdl::verify_stream_migration_automata_v1(&stream_capabilities, &automata)
        .map_err(|error| BundleError::new(BundleErrorCode::InvalidBundle, error))?;

    let mut findings = automata
        .iter()
        .flat_map(|(entity, automaton)| {
            lint_automaton(automaton)
                .into_iter()
                .map(|finding| BundleLintFinding {
                    entity: entity.clone(),
                    severity: finding.severity,
                    code: finding.code,
                    message: finding.message,
                })
        })
        .collect::<Vec<_>>();
    findings.extend(lint_automata_bundle(&automata));
    findings.extend(lint_automata_csdl_bundle(&automata, &csdl));
    findings.extend(lint_csdl_reference_contracts(&csdl, &automata));
    findings.sort_by(|left, right| {
        (&left.entity, &left.code, &left.message).cmp(&(&right.entity, &right.code, &right.message))
    });
    if let Some(finding) = findings
        .into_iter()
        .find(|finding| finding.severity == LintSeverity::Error)
    {
        return Err(BundleError::new(
            BundleErrorCode::InvalidBundle,
            format!("{}: {}: {}", finding.entity, finding.code, finding.message),
        ));
    }
    Ok(())
}

fn validate_scope(scope_id: &str) -> Result<(), BundleError> {
    if scope_id.trim().is_empty() || scope_id.len() > MAX_SCOPE_ID_BYTES {
        return Err(BundleError::new(
            BundleErrorCode::InvalidScope,
            format!("scope ID must contain 1..={MAX_SCOPE_ID_BYTES} UTF-8 bytes"),
        ));
    }
    Ok(())
}

fn validate_predecessor(predecessor: Option<&str>) -> Result<(), BundleError> {
    let Some(predecessor) = predecessor else {
        return Ok(());
    };
    let Some(hex) = predecessor.strip_prefix("sha256:") else {
        return Err(invalid_predecessor());
    };
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid_predecessor());
    }
    Ok(())
}

fn invalid_predecessor() -> BundleError {
    BundleError::new(
        BundleErrorCode::InvalidPredecessor,
        "predecessor must use lowercase sha256:<64 hex> form",
    )
}

fn canonical_ioa_specs(sources: Vec<IoaSourceInput>) -> Result<Vec<CanonicalIoaSpec>, BundleError> {
    let mut seen = BTreeSet::new();
    let mut canonical = Vec::with_capacity(sources.len());
    for source in sources {
        validate_qualified_name(&source.entity_type)?;
        if source.source.len() > MAX_IOA_SOURCE_BYTES {
            return Err(BundleError::new(
                BundleErrorCode::BudgetExceeded,
                format!(
                    "IOA source '{}' exceeds v1 byte budget {MAX_IOA_SOURCE_BYTES}",
                    source.entity_type
                ),
            ));
        }
        if !seen.insert(source.entity_type.clone()) {
            return Err(BundleError::new(
                BundleErrorCode::DuplicateSymbol,
                format!("duplicate IOA entity '{}'", source.entity_type),
            ));
        }
        let automaton = parse_automaton(&source.source).map_err(|error| {
            BundleError::new(
                BundleErrorCode::InvalidIoa,
                format!("failed to parse '{}': {error}", source.entity_type),
            )
        })?;
        let short_name = source.entity_type.rsplit('.').next().unwrap_or_default();
        if short_name != automaton.automaton.name {
            return Err(BundleError::new(
                BundleErrorCode::EntityNameMismatch,
                format!(
                    "submitted entity '{}' has short name '{short_name}', but its automaton declares '{}'",
                    source.entity_type, automaton.automaton.name
                ),
            ));
        }
        let value = toml::from_str::<toml::Value>(&source.source).map_err(|error| {
            BundleError::new(
                BundleErrorCode::InvalidIoa,
                format!("failed to canonicalize '{}': {error}", source.entity_type),
            )
        })?;
        let canonical_source = toml::to_string(&value).map_err(|error| {
            BundleError::new(
                BundleErrorCode::InvalidIoa,
                format!("failed to emit '{}': {error}", source.entity_type),
            )
        })?;
        canonical.push(CanonicalIoaSpec {
            entity_type: source.entity_type,
            canonical_source,
        });
    }
    canonical.sort_by(|left, right| left.entity_type.cmp(&right.entity_type));
    Ok(canonical)
}

fn validate_qualified_name(name: &str) -> Result<(), BundleError> {
    if name.trim() != name
        || name.len() > MAX_ENTITY_TYPE_BYTES
        || name.split('.').count() < 2
        || name.split('.').any(|segment| segment.is_empty())
    {
        return Err(BundleError::new(
            BundleErrorCode::EntityNameMismatch,
            format!("IOA entity key '{name}' must be a fully qualified name"),
        ));
    }
    Ok(())
}

fn canonical_policies(
    mut policies: Vec<PolicyArtifactInput>,
) -> Result<Vec<PolicyArtifactInput>, BundleError> {
    validate_artifact_count("Cedar policy", policies.len())?;
    let mut seen = BTreeSet::new();
    for policy in &mut policies {
        validate_artifact_name("Cedar policy", &policy.name)?;
        if !seen.insert(policy.name.as_str()) {
            return Err(BundleError::new(
                BundleErrorCode::DuplicateSymbol,
                format!("duplicate Cedar policy '{}'", policy.name),
            ));
        }
        if policy.source.len() > MAX_POLICY_SOURCE_BYTES {
            return Err(BundleError::new(
                BundleErrorCode::BudgetExceeded,
                format!(
                    "Cedar policy '{}' exceeds v1 byte budget {MAX_POLICY_SOURCE_BYTES}",
                    policy.name
                ),
            ));
        }
        policy.source = policy.source.replace("\r\n", "\n");
    }
    policies.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(policies)
}

fn canonical_wasm_modules(
    mut modules: Vec<WasmArtifactInput>,
) -> Result<Vec<WasmArtifactInput>, BundleError> {
    validate_artifact_count("WASM module", modules.len())?;
    let mut seen = BTreeSet::new();
    for module in &modules {
        validate_artifact_name("WASM module", &module.name)?;
        if !seen.insert(module.name.as_str()) {
            return Err(BundleError::new(
                BundleErrorCode::DuplicateSymbol,
                format!("duplicate WASM module '{}'", module.name),
            ));
        }
        validate_artifact_digest("WASM module", &module.artifact_digest)?;
        if let Some(binding_digest) = &module.data_binding_digest {
            validate_artifact_digest("WASM module data binding", binding_digest)?;
        }
    }
    modules.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(modules)
}

fn validate_migration(
    migration: Option<MigrationArtifactInput>,
) -> Result<Option<MigrationArtifactInput>, BundleError> {
    let Some(migration) = migration else {
        return Ok(None);
    };
    validate_artifact_name("migration module", &migration.name)?;
    validate_artifact_digest("migration module", &migration.artifact_digest)?;
    if migration.abi_version != "temper-schema-migration/v1" {
        return Err(BundleError::new(
            BundleErrorCode::InvalidMigration,
            format!("unsupported migration ABI '{}'", migration.abi_version),
        ));
    }
    Ok(Some(migration))
}

fn validate_artifact_count(kind: &str, count: usize) -> Result<(), BundleError> {
    if count > MAX_NAMED_ARTIFACTS {
        return Err(BundleError::new(
            BundleErrorCode::BudgetExceeded,
            format!("{kind} count {count} exceeds v1 budget {MAX_NAMED_ARTIFACTS}"),
        ));
    }
    Ok(())
}

fn validate_artifact_name(kind: &str, name: &str) -> Result<(), BundleError> {
    if name.trim().is_empty() || name.trim() != name || name.len() > MAX_ARTIFACT_NAME_BYTES {
        return Err(BundleError::new(
            BundleErrorCode::InvalidArtifact,
            format!("{kind} name must contain 1..={MAX_ARTIFACT_NAME_BYTES} UTF-8 bytes"),
        ));
    }
    Ok(())
}

fn validate_artifact_digest(kind: &str, digest: &str) -> Result<(), BundleError> {
    validate_predecessor(Some(digest)).map_err(|_| {
        BundleError::new(
            BundleErrorCode::InvalidArtifact,
            format!("{kind} digest must use lowercase sha256:<64 hex> form"),
        )
    })
}

fn validate_budgets(budgets: &ScopedBundleBudgets) -> Result<(), BundleError> {
    if budgets.verification_steps == 0
        || budgets.migration_fuel_per_entity == 0
        || budgets.migration_memory_pages == 0
        || budgets.migration_input_bytes == 0
        || budgets.migration_output_bytes == 0
        || budgets.migration_entities_per_batch == 0
        || budgets.migration_total_entities == 0
        || budgets.migration_total_batches == 0
        || budgets.migration_attempts == 0
        || u64::from(budgets.migration_entities_per_batch) > budgets.migration_total_entities
        || budgets.migration_total_batches > budgets.migration_total_entities
        || u64::from(budgets.migration_attempts) > budgets.migration_total_batches
    {
        return Err(BundleError::new(
            BundleErrorCode::BudgetExceeded,
            "scoped bundle budgets must be positive and internally consistent",
        ));
    }
    Ok(())
}
