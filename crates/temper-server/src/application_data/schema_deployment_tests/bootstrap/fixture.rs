use super::*;
use temper_spec::bundle::ScopedSpecBundle;

pub(super) fn bootstrap_bundle_request() -> SubmitSchemaBundleRequestV1 {
    let budgets = ScopedBundleBudgets::default();
    let csdl = super::super::super::tests::CSDL;
    let ioa = super::super::super::tests::IOA;
    let compiled = ScopedSpecBundle::compile(ScopedSpecBundleInput {
        scope_id: "bootstrap-e2e".into(),
        predecessor_digest: None,
        csdl_xml: csdl.into(),
        ioa_sources: vec![IoaSourceInput {
            entity_type: "Temper.Example.Customer".into(),
            source: ioa.into(),
        }],
        cedar_policies: vec![],
        wasm_modules: vec![],
        migration: None,
        budgets: budgets.clone(),
    })
    .expect("bootstrap fixture bundle should compile");
    SubmitSchemaBundleRequestV1 {
        request_id: "bootstrap-e2e-submit".into(),
        idempotency_key: "bootstrap-e2e-submit".into(),
        scope: SchemaScopeV1 {
            kind: "task".into(),
            id: "bootstrap-e2e".into(),
        },
        expected_predecessor: None,
        expected_digest: compiled.digest().into(),
        canonicalization_version: temper_spec::bundle::SCOPED_SPEC_BUNDLE_CONTRACT_V1.into(),
        csdl: csdl.into(),
        ioa: vec![SchemaIoaSourceV1 {
            entity_type: "Temper.Example.Customer".into(),
            source: ioa.into(),
        }],
        cedar_policies: vec![],
        wasm_modules: vec![],
        migration: None,
        budgets: SchemaBundleBudgetsV1 {
            verification_steps: budgets.verification_steps,
            migration_fuel_per_entity: budgets.migration_fuel_per_entity,
            migration_memory_pages: budgets.migration_memory_pages,
            migration_input_bytes: budgets.migration_input_bytes,
            migration_output_bytes: budgets.migration_output_bytes,
            migration_entities_per_batch: budgets.migration_entities_per_batch,
            migration_total_entities: budgets.migration_total_entities,
            migration_total_batches: budgets.migration_total_batches,
            migration_attempts: budgets.migration_attempts,
        },
    }
}
