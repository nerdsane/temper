//! Domain-separated, length-framed bundle identity.

use sha2::{Digest, Sha256};

use super::{
    CanonicalIoaSpec, MigrationArtifactInput, PolicyArtifactInput, SCOPED_SPEC_BUNDLE_CONTRACT_V1,
    ScopedBundleBudgets, WasmArtifactInput,
};

#[expect(
    clippy::too_many_arguments,
    reason = "the digest boundary enumerates every authoritative v1 section explicitly"
)]
pub(super) fn bundle_digest(
    scope_id: &str,
    predecessor: Option<&str>,
    canonical_csdl: &str,
    ioa_specs: &[CanonicalIoaSpec],
    cedar_policies: &[PolicyArtifactInput],
    wasm_modules: &[WasmArtifactInput],
    migration: Option<&MigrationArtifactInput>,
    budgets: &ScopedBundleBudgets,
) -> String {
    let mut hasher = Sha256::new();
    digest_frame(
        &mut hasher,
        b"contract",
        SCOPED_SPEC_BUNDLE_CONTRACT_V1.as_bytes(),
    );
    digest_frame(&mut hasher, b"scope_kind", b"task");
    digest_frame(&mut hasher, b"scope_id", scope_id.as_bytes());
    digest_frame(
        &mut hasher,
        b"predecessor",
        predecessor.unwrap_or("").as_bytes(),
    );
    digest_frame(&mut hasher, b"csdl", canonical_csdl.as_bytes());
    for spec in ioa_specs {
        digest_frame(&mut hasher, b"ioa_name", spec.entity_type.as_bytes());
        digest_frame(&mut hasher, b"ioa_source", spec.canonical_source.as_bytes());
    }
    for policy in cedar_policies {
        digest_frame(&mut hasher, b"cedar_name", policy.name.as_bytes());
        digest_frame(&mut hasher, b"cedar_source", policy.source.as_bytes());
    }
    for module in wasm_modules {
        digest_frame(&mut hasher, b"wasm_name", module.name.as_bytes());
        digest_frame(
            &mut hasher,
            b"wasm_digest",
            module.artifact_digest.as_bytes(),
        );
    }
    digest_migration(&mut hasher, migration);
    digest_budgets(&mut hasher, budgets);
    format!("sha256:{:x}", hasher.finalize())
}

fn digest_migration(hasher: &mut Sha256, migration: Option<&MigrationArtifactInput>) {
    if let Some(migration) = migration {
        digest_frame(hasher, b"migration_present", b"1");
        digest_frame(hasher, b"migration_name", migration.name.as_bytes());
        digest_frame(
            hasher,
            b"migration_digest",
            migration.artifact_digest.as_bytes(),
        );
        digest_frame(hasher, b"migration_abi", migration.abi_version.as_bytes());
    } else {
        digest_frame(hasher, b"migration_present", b"0");
    }
}

fn digest_budgets(hasher: &mut Sha256, budgets: &ScopedBundleBudgets) {
    digest_frame(
        hasher,
        b"budget_verification_steps",
        &budgets.verification_steps.to_be_bytes(),
    );
    digest_frame(
        hasher,
        b"budget_migration_fuel_per_entity",
        &budgets.migration_fuel_per_entity.to_be_bytes(),
    );
    digest_frame(
        hasher,
        b"budget_migration_memory_pages",
        &budgets.migration_memory_pages.to_be_bytes(),
    );
    digest_frame(
        hasher,
        b"budget_migration_input_bytes",
        &budgets.migration_input_bytes.to_be_bytes(),
    );
    digest_frame(
        hasher,
        b"budget_migration_output_bytes",
        &budgets.migration_output_bytes.to_be_bytes(),
    );
    digest_frame(
        hasher,
        b"budget_migration_entities_per_batch",
        &budgets.migration_entities_per_batch.to_be_bytes(),
    );
    digest_frame(
        hasher,
        b"budget_migration_total_entities",
        &budgets.migration_total_entities.to_be_bytes(),
    );
    digest_frame(
        hasher,
        b"budget_migration_total_batches",
        &budgets.migration_total_batches.to_be_bytes(),
    );
    digest_frame(
        hasher,
        b"budget_migration_attempts",
        &budgets.migration_attempts.to_be_bytes(),
    );
}

fn digest_frame(hasher: &mut Sha256, label: &[u8], value: &[u8]) {
    hasher.update((label.len() as u64).to_be_bytes());
    hasher.update(label);
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}
