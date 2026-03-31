//! WASM engine: compile, cache, and invoke modules.
//!
//! Modules are compiled once and cached by SHA-256 hash. Each invocation
//! gets a fresh `Store` with fuel + memory limits (TigerStyle budgets).

mod host_functions;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use sha2::{Digest, Sha256};
use wasmtime::{Config, Engine, Linker, Module, ResourceLimiter, Store};
use wasmtime_wasi::preview1::WasiP1Ctx;
use wasmtime_wasi::{WasiCtxBuilder, preview1};

use crate::host_trait::WasmHost;
use crate::stream::StreamRegistry;
use crate::types::{
    MAX_MODULE_SIZE, WasmInvocationContext, WasmInvocationResult, WasmResourceLimits,
};

/// Errors from the WASM engine.
#[derive(Debug, thiserror::Error)]
pub enum WasmError {
    /// Module exceeds the maximum allowed size.
    #[error("module too large: {size} bytes (max {max})")]
    ModuleTooLarge {
        /// Actual size of the module.
        size: usize,
        /// Maximum allowed size.
        max: usize,
    },
    /// WASM module compilation failed.
    #[error("compilation failed: {0}")]
    Compilation(String),
    /// WASM module instantiation failed.
    #[error("instantiation failed: {0}")]
    Instantiation(String),
    /// WASM function invocation failed.
    #[error("invocation failed: {0}")]
    Invocation(String),
    /// Module exceeded its instruction fuel budget.
    #[error("fuel exhausted -- module exceeded instruction budget")]
    FuelExhausted,
    /// Module exceeded its wall-clock execution timeout.
    #[error("execution timeout -- module exceeded time budget of {0:?}")]
    Timeout(std::time::Duration),
    /// Module attempted to exceed its memory budget.
    #[error("memory limit exceeded -- module requested more than {max_bytes} bytes")]
    MemoryLimitExceeded {
        /// Configured memory limit in bytes.
        max_bytes: usize,
    },
    /// Requested module hash not found in cache.
    #[error("module not found: {0}")]
    ModuleNotFound(String),
}

/// Memory limiter enforcing a per-invocation byte cap via Wasmtime's ResourceLimiter.
///
/// Passed into each Store so that `memory.grow` instructions that would exceed
/// `max_memory` are denied. On denial the module receives a failed grow (returns
/// -1 from `memory.grow`); if the trap-on-deny path is used it raises a trap.
struct MemoryLimiter {
    /// Maximum allowed linear memory in bytes.
    max_memory: usize,
}

impl ResourceLimiter for MemoryLimiter {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> Result<bool, wasmtime::Error> {
        if desired > self.max_memory {
            // Return Ok(false) so memory.grow returns -1 (spec-compliant denial).
            Ok(false)
        } else {
            Ok(true)
        }
    }

    fn table_growing(
        &mut self,
        _current: usize,
        _desired: usize,
        _maximum: Option<usize>,
    ) -> Result<bool, wasmtime::Error> {
        Ok(true)
    }
}

/// Compiled module cache entry.
struct CachedModule {
    /// The compiled wasmtime module.
    module: Module,
}

/// Host state passed into the WASM store.
pub(crate) struct HostState {
    /// Serialized invocation context JSON.
    pub(crate) context_json: String,
    /// Result JSON set by the guest via host_set_result.
    pub(crate) result_json: Option<String>,
    /// Host capabilities (HTTP, secrets, logging).
    pub(crate) host: Arc<dyn WasmHost>,
    /// Memory limiter enforcing `max_memory` per invocation.
    limiter: MemoryLimiter,
    /// Stream registry for binary data transfer between host and WASM guest.
    /// Bytes never enter WASM memory — WASM references them by stream ID.
    pub(crate) streams: Arc<RwLock<StreamRegistry>>,
    /// WASI context for modules compiled with wasm32-wasi target.
    /// None for wasm32-unknown-unknown modules. When present, WASI
    /// syscalls (clock_time_get, random_get, etc.) are available.
    pub(crate) wasi_ctx: Option<WasiP1Ctx>,
}

/// WASM engine: compile, cache, invoke modules.
///
/// Modules are compiled once and cached by SHA-256 hash. Each invocation
/// gets a fresh `Store` with fuel + memory limits (TigerStyle budgets).
pub struct WasmEngine {
    /// The underlying wasmtime engine.
    engine: Engine,
    /// Compiled module cache: SHA-256 hash -> compiled module.
    cache: RwLock<BTreeMap<String, Arc<CachedModule>>>,
}

impl WasmEngine {
    /// Create a new WASM engine with fuel metering and epoch interruption enabled.
    pub fn new() -> Result<Self, WasmError> {
        let mut config = Config::new();
        config.consume_fuel(true);
        config.epoch_interruption(true);
        config.wasm_component_model(true);

        let engine = Engine::new(&config).map_err(|e| WasmError::Compilation(e.to_string()))?;

        Ok(Self {
            engine,
            cache: RwLock::new(BTreeMap::new()),
        })
    }

    /// Compute SHA-256 hash of WASM bytes.
    pub fn hash_module(wasm_bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(wasm_bytes);
        format!("{:x}", hasher.finalize())
    }

    /// Compile and cache a WASM module.
    ///
    /// Returns the SHA-256 hash of the module bytes.
    pub fn compile_and_cache(&self, wasm_bytes: &[u8]) -> Result<String, WasmError> {
        // TigerStyle: pre-assertion on module size
        if wasm_bytes.len() > MAX_MODULE_SIZE {
            return Err(WasmError::ModuleTooLarge {
                size: wasm_bytes.len(),
                max: MAX_MODULE_SIZE,
            });
        }

        let hash = Self::hash_module(wasm_bytes);

        // Check if already cached
        {
            let cache = self.cache.read().expect("cache lock poisoned");
            if cache.contains_key(&hash) {
                return Ok(hash);
            }
        }

        let module = Module::new(&self.engine, wasm_bytes)
            .map_err(|e| WasmError::Compilation(e.to_string()))?;

        let cached = Arc::new(CachedModule { module });
        {
            let mut cache = self.cache.write().expect("cache lock poisoned");
            cache.insert(hash.clone(), cached);
        }

        tracing::info!(hash = %hash, size = wasm_bytes.len(), "WASM module compiled and cached");
        Ok(hash)
    }

    /// Check if a module is cached.
    pub fn is_cached(&self, hash: &str) -> bool {
        let cache = self.cache.read().expect("cache lock poisoned");
        cache.contains_key(hash)
    }

    /// Invoke a cached WASM module.
    ///
    /// Each invocation gets a fresh Store with fuel and memory limits.
    /// The module must export a `run` function that takes `(i32, i32)` ->
    /// `i32` where the inputs are (context_ptr, context_len) and the return
    /// is a result pointer. Alternatively, the module can use `host_set_result`
    /// to provide the result via host call.
    pub async fn invoke(
        &self,
        module_hash: &str,
        context: &WasmInvocationContext,
        host: Arc<dyn WasmHost>,
        limits: &WasmResourceLimits,
        streams: Arc<RwLock<StreamRegistry>>,
    ) -> Result<WasmInvocationResult, WasmError> {
        let cached = {
            let cache = self.cache.read().expect("cache lock poisoned");
            cache
                .get(module_hash)
                .cloned()
                .ok_or_else(|| WasmError::ModuleNotFound(module_hash.to_string()))?
        };

        let start = std::time::Instant::now(); // determinism-ok: wall-clock timing for WASM sandbox
        let context_json = serde_json::to_string(context)
            .map_err(|e| WasmError::Invocation(format!("failed to serialize context: {e}")))?;

        // Create a fresh store with fuel budget and memory limiter
        // Check if the module imports wasi_snapshot_preview1 (wasm32-wasi target).
        let needs_wasi = cached
            .module
            .imports()
            .any(|imp| imp.module() == "wasi_snapshot_preview1");

        let wasi_ctx = if needs_wasi {
            // Minimal WASI context: clock + random, no filesystem or network.
            let wasi = WasiCtxBuilder::new().build_p1();
            Some(wasi)
        } else {
            None
        };

        let host_state = HostState {
            context_json: context_json.clone(),
            result_json: None,
            host,
            limiter: MemoryLimiter {
                max_memory: limits.max_memory,
            },
            streams,
            wasi_ctx,
        };
        let mut store = Store::new(&self.engine, host_state);
        store
            .set_fuel(limits.max_fuel)
            .map_err(|e| WasmError::Invocation(format!("failed to set fuel: {e}")))?;

        // Register the memory limiter so memory.grow is gated by max_memory.
        store.limiter(|state| &mut state.limiter);

        // Set epoch deadline to 1 tick — the engine epoch is incremented by
        // the timeout task below. If the task fires before run() returns, the
        // module receives a trap on the next back-edge check.
        store.set_epoch_deadline(1);

        // Spawn a one-shot timer that increments the epoch after max_duration.
        // This provides wall-clock timeout on top of the fuel instruction budget.
        let engine_for_timeout = self.engine.clone();
        let max_duration = limits.max_duration;
        let timeout_task = tokio::spawn(async move {
            // determinism-ok: epoch timer for WASM wall-clock timeout enforcement
            tokio::time::sleep(max_duration).await;
            engine_for_timeout.increment_epoch();
        });

        // Guard that aborts the epoch timer on any exit path (Ok or Err).
        // This prevents a leaked timer from perturbing concurrent invocations.
        struct AbortOnDrop(tokio::task::JoinHandle<()>);
        impl Drop for AbortOnDrop {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        let _timer_guard = AbortOnDrop(timeout_task);

        // Link host functions
        let mut linker = Linker::new(&self.engine);
        host_functions::link_host_functions(&mut linker)?;

        // Link WASI imports for wasm32-wasi modules (clock, random, etc.).
        // Non-WASI modules skip this — their imports are fully satisfied by
        // the custom host functions above.
        if needs_wasi {
            preview1::add_to_linker_sync(&mut linker, |state: &mut HostState| {
                state
                    .wasi_ctx
                    .as_mut()
                    .expect("wasi_ctx must be Some when needs_wasi is true")
            })
            .map_err(|e| WasmError::Compilation(format!("failed to link WASI: {e}")))?;
        }

        // Instantiate
        let instance = linker
            .instantiate(&mut store, &cached.module)
            .map_err(|e| WasmError::Instantiation(e.to_string()))?;

        // Find and call the `run` export
        let run_fn = instance
            .get_typed_func::<(i32, i32), i32>(&mut store, "run")
            .map_err(|e| WasmError::Invocation(format!("module missing 'run' export: {e}")))?;

        // Write context JSON into module memory
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| WasmError::Invocation("module missing 'memory' export".into()))?;

        let ctx_bytes = context_json.as_bytes();
        let ctx_ptr = 1024_usize; // Fixed offset for context data
        memory.write(&mut store, ctx_ptr, ctx_bytes).map_err(|e| {
            WasmError::Invocation(format!("failed to write context to memory: {e}"))
        })?;

        // Call run(ptr, len) -> result_ptr
        let result_ptr = run_fn
            .call(&mut store, (ctx_ptr as i32, ctx_bytes.len() as i32))
            .map_err(|e| {
                // Use downcast to identify trap kind — the display string wraps
                // backtrace context so string matching is unreliable.
                match e.downcast_ref::<wasmtime::Trap>() {
                    Some(&wasmtime::Trap::OutOfFuel) => WasmError::FuelExhausted,
                    Some(&wasmtime::Trap::Interrupt) => WasmError::Timeout(max_duration),
                    _ => WasmError::Invocation(e.to_string()),
                }
            })?;

        let duration_ms = start.elapsed().as_millis() as u64;

        // Read result: prefer host_set_result (explicit API), fall back to memory pointer.
        let result_json = if let Some(ref host_result) = store.data().result_json {
            // Module used host_set_result — this is the preferred path.
            host_result.clone()
        } else if result_ptr > 0 {
            // Legacy path: read from module memory at result_ptr with length at result_ptr-4.
            let mut len_bytes = [0u8; 4];
            memory
                .read(&store, (result_ptr - 4) as usize, &mut len_bytes)
                .map_err(|e| WasmError::Invocation(format!("failed to read result length: {e}")))?;
            let result_len = u32::from_le_bytes(len_bytes) as usize;

            let mut result_bytes = vec![0u8; result_len];
            memory
                .read(&store, result_ptr as usize, &mut result_bytes)
                .map_err(|e| WasmError::Invocation(format!("failed to read result: {e}")))?;

            String::from_utf8(result_bytes)
                .map_err(|e| WasmError::Invocation(format!("result is not valid UTF-8: {e}")))?
        } else {
            // No result from either path.
            String::new()
        };

        // Parse the result JSON
        if result_json.is_empty() {
            return Ok(WasmInvocationResult {
                callback_action: String::new(),
                callback_params: serde_json::Value::Null,
                success: false,
                error: Some("module returned empty result".to_string()),
                duration_ms,
            });
        }

        let parsed: serde_json::Value = serde_json::from_str(&result_json)
            .map_err(|e| WasmError::Invocation(format!("failed to parse result JSON: {e}")))?;

        Ok(WasmInvocationResult {
            callback_action: parsed
                .get("action")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            callback_params: parsed
                .get("params")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
            success: parsed
                .get("success")
                .and_then(|v| v.as_bool())
                .unwrap_or(true),
            error: parsed
                .get("error")
                .and_then(|v| v.as_str())
                .map(String::from),
            duration_ms,
        })
    }

    /// Remove a module from the cache.
    pub fn evict(&self, hash: &str) -> bool {
        let mut cache = self.cache.write().expect("cache lock poisoned");
        cache.remove(hash).is_some()
    }

    /// Number of cached modules.
    pub fn cache_size(&self) -> usize {
        let cache = self.cache.read().expect("cache lock poisoned");
        cache.len()
    }
}

impl Default for WasmEngine {
    fn default() -> Self {
        Self::new().expect("failed to create default WasmEngine")
    }
}
