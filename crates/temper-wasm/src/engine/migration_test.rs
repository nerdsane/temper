use std::time::Duration;

use temper_wasm_sdk::schema_deployment::{SchemaMigrationInputV1, SchemaMigrationLogicalContextV1};

use super::{PureMigrationError, PureMigrationLimits, WasmEngine, validate_canonical_state};

fn limits() -> PureMigrationLimits {
    PureMigrationLimits {
        max_fuel: 100_000,
        max_memory_pages: 2,
        max_input_bytes: 4_096,
        max_output_bytes: 4_096,
        max_duration: Duration::from_secs(1),
    }
}

fn input() -> SchemaMigrationInputV1 {
    SchemaMigrationInputV1 {
        abi_version: 1,
        source_bundle_digest: format!("sha256:{}", "1".repeat(64)),
        target_bundle_digest: format!("sha256:{}", "2".repeat(64)),
        entity_type: "Example.Order".into(),
        entity_id: "order-1".into(),
        source_sequence: 7,
        canonical_state_json: r#"{"Id":"order-1"}"#.into(),
        logical_context: SchemaMigrationLogicalContextV1 {
            batch_id: "batch-1".into(),
            item_index: 0,
        },
    }
}

#[test]
fn rejects_missing_or_wrongly_typed_exports() {
    let engine = WasmEngine::new().expect("engine");
    let missing_memory = br#"
        (module
          (func (export "temper_schema_alloc_v1") (param i32) (result i32) i32.const 1)
          (func (export "temper_schema_migrate_v1") (param i32 i32) (result i64) i64.const 0))
    "#;
    let hash = engine.compile_and_cache(missing_memory).expect("compile");
    assert!(matches!(
        engine.verify_pure_migration_module(&hash, limits()),
        Err(PureMigrationError::Rejected(message)) if message.contains("missing memory")
    ));

    let wrong_migrate = br#"
        (module
          (memory (export "memory") 1)
          (func (export "temper_schema_alloc_v1") (param i32) (result i32) i32.const 1)
          (func (export "temper_schema_migrate_v1") (param i32) (result i32) i32.const 0))
    "#;
    let hash = engine.compile_and_cache(wrong_migrate).expect("compile");
    assert!(matches!(
        engine.verify_pure_migration_module(&hash, limits()),
        Err(PureMigrationError::Rejected(message)) if message.contains("invalid migration export")
    ));
}

#[test]
fn reports_traps_and_fuel_exhaustion_with_stable_classes() {
    let engine = WasmEngine::new().expect("engine");
    let trap = br#"
        (module
          (memory (export "memory") 1)
          (func (export "temper_schema_alloc_v1") (param i32) (result i32) i32.const 1024)
          (func (export "temper_schema_migrate_v1") (param i32 i32) (result i64) unreachable))
    "#;
    let hash = engine.compile_and_cache(trap).expect("compile");
    assert!(matches!(
        engine.invoke_pure_migration(&hash, &input(), limits()),
        Err(PureMigrationError::Failed(_))
    ));

    let spin = br#"
        (module
          (memory (export "memory") 1)
          (func (export "temper_schema_alloc_v1") (param i32) (result i32) i32.const 1024)
          (func (export "temper_schema_migrate_v1") (param i32 i32) (result i64)
            (loop $forever br $forever) i64.const 0))
    "#;
    let hash = engine.compile_and_cache(spin).expect("compile");
    let mut fuel_limited = limits();
    fuel_limited.max_fuel = 100;
    let fuel_result = engine.invoke_pure_migration(&hash, &input(), fuel_limited);
    assert!(
        matches!(
            &fuel_result,
            Err(PureMigrationError::BudgetExhausted(message)) if message.contains("fuel")
        ),
        "{fuel_result:?}"
    );
}

#[test]
fn rejects_memory_growth_and_malformed_output() {
    let engine = WasmEngine::new().expect("engine");
    let grow = br#"
        (module
          (memory (export "memory") 3)
          (func (export "temper_schema_alloc_v1") (param i32) (result i32) i32.const 1024)
          (func (export "temper_schema_migrate_v1") (param i32 i32) (result i64) i64.const 0))
    "#;
    let hash = engine.compile_and_cache(grow).expect("compile");
    let memory_result = engine.invoke_pure_migration(&hash, &input(), limits());
    assert!(
        matches!(
            &memory_result,
            Err(PureMigrationError::BudgetExhausted(message)) if message.contains("memory")
        ),
        "{memory_result:?}"
    );

    let malformed = br#"
        (module
          (memory (export "memory") 1)
          (data (i32.const 4096) "not-json")
          (func (export "temper_schema_alloc_v1") (param i32) (result i32) i32.const 1024)
          (func (export "temper_schema_migrate_v1") (param i32 i32) (result i64)
            i64.const 17592186044424))
    "#;
    let hash = engine.compile_and_cache(malformed).expect("compile");
    assert!(matches!(
        engine.invoke_pure_migration(&hash, &input(), limits()),
        Err(PureMigrationError::Rejected(message)) if message.contains("invalid output")
    ));
}

#[test]
fn canonical_state_requires_recursively_sorted_object_keys() {
    assert!(validate_canonical_state(r#"{"a":{"b":2,"c":3},"z":1}"#).is_ok());
    assert!(matches!(
        validate_canonical_state(r#"{"z":1,"a":{"c":3,"b":2}}"#),
        Err(PureMigrationError::Rejected(message)) if message.contains("not canonical")
    ));
    assert!(matches!(
        validate_canonical_state("[]"),
        Err(PureMigrationError::Rejected(message)) if message.contains("must be an object")
    ));
}
