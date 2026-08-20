//! Closed deterministic schema-migration ABI.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use temper_wasm_sdk::schema_deployment::{SchemaMigrationInputV1, SchemaMigrationOutputV1};
use wasmtime::{Linker, Store};

use super::{MemoryLimiter, WasmEngine, epoch_deadline_ticks};

const WASM_PAGE_BYTES: usize = 65_536;
const MAX_REJECT_CODE_BYTES: usize = 128;
const MAX_REJECT_MESSAGE_BYTES: usize = 1_024;

/// Explicit budgets for one pure schema migration invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PureMigrationLimits {
    /// Wasmtime instruction fuel available to the invocation.
    pub max_fuel: u64,
    /// Maximum linear-memory pages (64 KiB each).
    pub max_memory_pages: u32,
    /// Maximum encoded input size.
    pub max_input_bytes: u32,
    /// Maximum encoded output size.
    pub max_output_bytes: u32,
    /// Epoch interruption deadline used only as a fail-safe scheduler budget.
    pub max_duration: Duration,
}

impl PureMigrationLimits {
    fn validate(self) -> Result<Self, PureMigrationError> {
        if self.max_fuel == 0
            || self.max_memory_pages == 0
            || self.max_input_bytes == 0
            || self.max_output_bytes == 0
            || self.max_duration.is_zero()
        {
            return Err(PureMigrationError::BudgetExhausted(
                "migration budgets must be positive".into(),
            ));
        }
        Ok(self)
    }

    fn max_memory_bytes(self) -> Result<usize, PureMigrationError> {
        usize::try_from(self.max_memory_pages)
            .ok()
            .and_then(|pages| pages.checked_mul(WASM_PAGE_BYTES))
            .ok_or_else(|| {
                PureMigrationError::BudgetExhausted("migration memory budget overflowed".into())
            })
    }
}

/// Stable pure-migration validation and execution failures.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PureMigrationError {
    /// Module imports capabilities forbidden to migrations or has a wrong ABI.
    #[error("migration_rejected: {0}")]
    Rejected(String),
    /// An explicit execution or byte budget was consumed.
    #[error("migration_budget_exhausted: {0}")]
    BudgetExhausted(String),
    /// The module trapped or returned unreadable output.
    #[error("migration_failed: {0}")]
    Failed(String),
    /// The immutable module is not present in the engine cache.
    #[error("migration module not found: {0}")]
    ModuleNotFound(String),
}

struct MigrationStoreState {
    limiter: MemoryLimiter,
}

impl WasmEngine {
    /// Validate that a cached module exposes only the closed v1 migration ABI.
    pub fn verify_pure_migration_module(
        &self,
        module_hash: &str,
        limits: PureMigrationLimits,
    ) -> Result<(), PureMigrationError> {
        let limits = limits.validate()?;
        let cached = self.migration_module(module_hash)?;
        reject_imports(&cached.module)?;

        let mut store = Store::new(
            &self.engine,
            MigrationStoreState {
                limiter: MemoryLimiter {
                    max_memory: limits.max_memory_bytes()?,
                },
            },
        );
        store
            .set_fuel(limits.max_fuel)
            .map_err(|error| PureMigrationError::Failed(error.to_string()))?;
        store.limiter(|state| &mut state.limiter);
        store.set_epoch_deadline(epoch_deadline_ticks(limits.max_duration));
        let instance = Linker::new(&self.engine)
            .instantiate(&mut store, &cached.module)
            .map_err(map_wasmtime_error)?;
        bind_exports(&instance, &mut store)?;
        Ok(())
    }

    /// Invoke a cached pure migration module with no WASI or Temper host imports.
    pub fn invoke_pure_migration(
        &self,
        module_hash: &str,
        input: &SchemaMigrationInputV1,
        limits: PureMigrationLimits,
    ) -> Result<SchemaMigrationOutputV1, PureMigrationError> {
        let limits = limits.validate()?;
        validate_input(input)?;
        let input_bytes = serde_json::to_vec(input)
            .map_err(|error| PureMigrationError::Rejected(error.to_string()))?;
        if input_bytes.len() > limits.max_input_bytes as usize {
            return Err(PureMigrationError::BudgetExhausted(
                "migration input byte budget exhausted".into(),
            ));
        }

        let cached = self.migration_module(module_hash)?;
        reject_imports(&cached.module)?;
        let mut store = Store::new(
            &self.engine,
            MigrationStoreState {
                limiter: MemoryLimiter {
                    max_memory: limits.max_memory_bytes()?,
                },
            },
        );
        store
            .set_fuel(limits.max_fuel)
            .map_err(|error| PureMigrationError::Failed(error.to_string()))?;
        store.limiter(|state| &mut state.limiter);
        store.set_epoch_deadline(epoch_deadline_ticks(limits.max_duration));
        let instance = Linker::new(&self.engine)
            .instantiate(&mut store, &cached.module)
            .map_err(map_wasmtime_error)?;
        let (memory, alloc, migrate, dealloc) = bind_exports(&instance, &mut store)?;

        let input_len = i32::try_from(input_bytes.len()).map_err(|_| {
            PureMigrationError::BudgetExhausted("migration input length overflowed".into())
        })?;
        let input_ptr = alloc
            .call(&mut store, input_len)
            .map_err(map_wasmtime_error)?;
        if input_ptr <= 0 {
            return Err(PureMigrationError::Failed(
                "migration allocator returned an invalid pointer".into(),
            ));
        }
        memory
            .write(&mut store, input_ptr as usize, &input_bytes)
            .map_err(|error| PureMigrationError::Failed(error.to_string()))?;

        let packed = migrate
            .call(&mut store, (input_ptr, input_len))
            .map_err(map_wasmtime_error)?;
        if let Some(dealloc) = dealloc {
            dealloc
                .call(&mut store, (input_ptr, input_len))
                .map_err(map_wasmtime_error)?;
        }
        let output_ptr = (packed >> 32) as i32;
        let output_len = (packed as u64 & u64::from(u32::MAX)) as u32;
        if output_ptr <= 0 {
            return Err(PureMigrationError::Failed(
                "migration returned an invalid output pointer".into(),
            ));
        }
        if output_len == 0 || output_len > limits.max_output_bytes {
            return Err(PureMigrationError::BudgetExhausted(
                "migration output byte budget exhausted".into(),
            ));
        }
        let mut output_bytes = vec![0; output_len as usize];
        memory
            .read(&store, output_ptr as usize, &mut output_bytes)
            .map_err(|error| PureMigrationError::Failed(error.to_string()))?;
        let output: SchemaMigrationOutputV1 = serde_json::from_slice(&output_bytes)
            .map_err(|error| PureMigrationError::Rejected(format!("invalid output: {error}")))?;
        validate_output(&output)?;
        Ok(output)
    }

    fn migration_module(
        &self,
        module_hash: &str,
    ) -> Result<Arc<super::CachedModule>, PureMigrationError> {
        self.cache
            .read()
            .expect("cache lock poisoned")
            .get(module_hash)
            .cloned()
            .ok_or_else(|| PureMigrationError::ModuleNotFound(module_hash.to_string()))
    }
}

type MigrationExports = (
    wasmtime::Memory,
    wasmtime::TypedFunc<i32, i32>,
    wasmtime::TypedFunc<(i32, i32), i64>,
    Option<wasmtime::TypedFunc<(i32, i32), ()>>,
);

fn bind_exports(
    instance: &wasmtime::Instance,
    store: &mut Store<MigrationStoreState>,
) -> Result<MigrationExports, PureMigrationError> {
    let memory = instance
        .get_memory(&mut *store, "memory")
        .ok_or_else(|| PureMigrationError::Rejected("missing memory export".into()))?;
    let alloc = instance
        .get_typed_func::<i32, i32>(&mut *store, "temper_schema_alloc_v1")
        .map_err(|error| PureMigrationError::Rejected(format!("invalid allocator: {error}")))?;
    let migrate = instance
        .get_typed_func::<(i32, i32), i64>(&mut *store, "temper_schema_migrate_v1")
        .map_err(|error| {
            PureMigrationError::Rejected(format!("invalid migration export: {error}"))
        })?;
    let dealloc = match instance.get_func(&mut *store, "temper_schema_dealloc_v1") {
        Some(function) => Some(function.typed::<(i32, i32), ()>(&*store).map_err(|error| {
            PureMigrationError::Rejected(format!("invalid deallocator: {error}"))
        })?),
        None => None,
    };
    Ok((memory, alloc, migrate, dealloc))
}

fn reject_imports(module: &wasmtime::Module) -> Result<(), PureMigrationError> {
    if let Some(import) = module.imports().next() {
        return Err(PureMigrationError::Rejected(format!(
            "forbidden import {}::{}",
            import.module(),
            import.name()
        )));
    }
    Ok(())
}

fn validate_input(input: &SchemaMigrationInputV1) -> Result<(), PureMigrationError> {
    if input.abi_version != 1 {
        return Err(PureMigrationError::Rejected(
            "unsupported migration ABI version".into(),
        ));
    }
    for (name, value) in [
        ("source bundle digest", input.source_bundle_digest.as_str()),
        ("target bundle digest", input.target_bundle_digest.as_str()),
        ("entity type", input.entity_type.as_str()),
        ("entity id", input.entity_id.as_str()),
        ("batch id", input.logical_context.batch_id.as_str()),
    ] {
        if value.is_empty() || value.trim() != value {
            return Err(PureMigrationError::Rejected(format!(
                "{name} must be canonical and non-empty"
            )));
        }
    }
    validate_canonical_state(&input.canonical_state_json)
}

fn validate_output(output: &SchemaMigrationOutputV1) -> Result<(), PureMigrationError> {
    match output {
        SchemaMigrationOutputV1::Unchanged => Ok(()),
        SchemaMigrationOutputV1::Replace {
            canonical_state_json,
        } => validate_canonical_state(canonical_state_json),
        SchemaMigrationOutputV1::Reject { code, message } => {
            if code.is_empty()
                || code.trim() != code
                || code.len() > MAX_REJECT_CODE_BYTES
                || message.len() > MAX_REJECT_MESSAGE_BYTES
            {
                return Err(PureMigrationError::Rejected(
                    "migration reject diagnostic is not canonical or exceeds its budget".into(),
                ));
            }
            Ok(())
        }
    }
}

fn validate_canonical_state(json: &str) -> Result<(), PureMigrationError> {
    let value: Value = serde_json::from_str(json)
        .map_err(|error| PureMigrationError::Rejected(format!("invalid state JSON: {error}")))?;
    if !value.is_object() {
        return Err(PureMigrationError::Rejected(
            "canonical state JSON must be an object".into(),
        ));
    }
    let canonical = serde_json::to_string(&canonicalize_json(&value))
        .map_err(|error| PureMigrationError::Rejected(error.to_string()))?;
    if canonical != json {
        return Err(PureMigrationError::Rejected(
            "state JSON is not canonical".into(),
        ));
    }
    Ok(())
}

fn canonicalize_json(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let sorted = object
                .iter()
                .map(|(key, value)| (key.clone(), canonicalize_json(value)))
                .collect::<std::collections::BTreeMap<_, _>>();
            Value::Object(sorted.into_iter().collect())
        }
        Value::Array(values) => Value::Array(values.iter().map(canonicalize_json).collect()),
        scalar => scalar.clone(),
    }
}

fn map_wasmtime_error(error: wasmtime::Error) -> PureMigrationError {
    let message = format!("{error:#}");
    if message.contains("fuel") {
        PureMigrationError::BudgetExhausted("migration fuel budget exhausted".into())
    } else if message.contains("memory") || message.contains("out of bounds") {
        PureMigrationError::BudgetExhausted("migration memory budget exhausted".into())
    } else {
        PureMigrationError::Failed(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use temper_wasm_sdk::schema_deployment::SchemaMigrationLogicalContextV1;

    const UNCHANGED_MODULE: &[u8] = br#"
        (module
          (memory (export "memory") 1)
          (data (i32.const 4096) "{\22outcome\22:\22unchanged\22}")
          (func (export "temper_schema_alloc_v1") (param i32) (result i32)
            i32.const 1024)
          (func (export "temper_schema_dealloc_v1") (param i32 i32))
          (func (export "temper_schema_migrate_v1") (param i32 i32) (result i64)
            i64.const 17592186044439))
    "#;

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
            canonical_state_json: r#"{"Id":"order-1","State":"Open"}"#.into(),
            logical_context: SchemaMigrationLogicalContextV1 {
                batch_id: "batch-1".into(),
                item_index: 0,
            },
        }
    }

    #[test]
    fn pure_migration_invokes_closed_typed_abi() {
        let engine = WasmEngine::new().expect("engine");
        let hash = engine
            .compile_and_cache(UNCHANGED_MODULE)
            .expect("compile migration fixture");
        engine
            .verify_pure_migration_module(&hash, limits())
            .expect("verify migration ABI");
        assert_eq!(
            engine
                .invoke_pure_migration(&hash, &input(), limits())
                .expect("invoke migration"),
            SchemaMigrationOutputV1::Unchanged
        );
    }

    #[test]
    fn pure_migration_rejects_wasi_and_noncanonical_state() {
        let engine = WasmEngine::new().expect("engine");
        let wasi = br#"
            (module
              (import "wasi_snapshot_preview1" "random_get"
                (func $random_get (param i32 i32) (result i32)))
              (memory (export "memory") 1)
              (func (export "temper_schema_alloc_v1") (param i32) (result i32) i32.const 1)
              (func (export "temper_schema_migrate_v1") (param i32 i32) (result i64) i64.const 0))
        "#;
        let hash = engine
            .compile_and_cache(wasi)
            .expect("compile WASI fixture");
        assert!(matches!(
            engine.verify_pure_migration_module(&hash, limits()),
            Err(PureMigrationError::Rejected(message)) if message.contains("forbidden import")
        ));

        let hash = engine
            .compile_and_cache(UNCHANGED_MODULE)
            .expect("compile migration fixture");
        let mut noncanonical = input();
        noncanonical.canonical_state_json = "{ \"Id\": \"order-1\" }".into();
        assert!(matches!(
            engine.invoke_pure_migration(&hash, &noncanonical, limits()),
            Err(PureMigrationError::Rejected(message)) if message.contains("not canonical")
        ));
    }

    #[test]
    fn pure_migration_rejects_zero_and_output_byte_budgets() {
        let engine = WasmEngine::new().expect("engine");
        let hash = engine
            .compile_and_cache(UNCHANGED_MODULE)
            .expect("compile migration fixture");
        let mut zero = limits();
        zero.max_fuel = 0;
        assert!(matches!(
            engine.verify_pure_migration_module(&hash, zero),
            Err(PureMigrationError::BudgetExhausted(_))
        ));

        let mut too_small = limits();
        too_small.max_output_bytes = 22;
        assert!(matches!(
            engine.invoke_pure_migration(&hash, &input(), too_small),
            Err(PureMigrationError::BudgetExhausted(message)) if message.contains("output")
        ));
    }
}

#[cfg(test)]
#[path = "migration_test.rs"]
mod negative_tests;
