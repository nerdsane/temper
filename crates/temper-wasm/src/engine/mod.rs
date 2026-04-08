//! WASM engine: compile, cache, and invoke modules.
//!
//! Modules are compiled once and cached by SHA-256 hash. Each invocation
//! gets a fresh `Store` with fuel + memory limits (TigerStyle budgets).

mod host_functions;
mod telemetry;
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
    /// Pre-linked instance template for non-WASI modules.
    instance_pre: Option<wasmtime::InstancePre<HostState>>,
    /// Pre-linked instance template for WASI modules.
    instance_pre_wasi: Option<wasmtime::InstancePre<HostState>>,
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

        // Pre-link for both WASI and non-WASI paths.
        let needs_wasi = module
            .imports()
            .any(|imp| imp.module() == "wasi_snapshot_preview1");

        let (instance_pre, instance_pre_wasi) = if needs_wasi {
            let mut linker = Linker::new(&self.engine);
            host_functions::link_host_functions(&mut linker)
                .map_err(|e| WasmError::Compilation(format!("pre-link host functions: {e}")))?;
            preview1::add_to_linker_sync(&mut linker, |state: &mut HostState| {
                state.wasi_ctx.as_mut().expect("wasi_ctx must be Some")
            })
            .map_err(|e| WasmError::Compilation(format!("pre-link WASI: {e}")))?;
            let pre = linker
                .instantiate_pre(&module)
                .map_err(|e| WasmError::Compilation(format!("pre-instantiate WASI: {e}")))?;
            (None, Some(pre))
        } else {
            let mut linker = Linker::new(&self.engine);
            host_functions::link_host_functions(&mut linker)
                .map_err(|e| WasmError::Compilation(format!("pre-link host functions: {e}")))?;
            let pre = linker
                .instantiate_pre(&module)
                .map_err(|e| WasmError::Compilation(format!("pre-instantiate: {e}")))?;
            (Some(pre), None)
        };

        let cached = Arc::new(CachedModule {
            module,
            instance_pre,
            instance_pre_wasi,
        });
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
    #[tracing::instrument(
        name = "wasm.invoke",
        skip(self, context, host, limits, streams),
        fields(
            otel.name = "wasm.invoke",
            tenant = %context.tenant,
            entity_type = %context.entity_type,
            entity_id = %context.entity_id,
            trigger_action = %context.trigger_action,
            module_hash = %module_hash,
            agent_id = tracing::field::Empty,
            session_id = tracing::field::Empty,
            max_fuel = limits.max_fuel,
            max_memory = limits.max_memory as u64,
            max_response_bytes = limits.max_response_bytes as u64,
            stream_count_before = tracing::field::Empty,
            stream_count_after = tracing::field::Empty,
            needs_wasi = tracing::field::Empty,
            success = tracing::field::Empty,
            callback_action = tracing::field::Empty,
            duration_ms = tracing::field::Empty,
            error = tracing::field::Empty,
        )
    )]
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

        let engine = self.engine.clone();
        let context_owned = context.clone();
        let limits_owned = limits.clone();
        let span = tracing::Span::current();

        tokio::task::spawn_blocking(move || {
            let _entered = span.enter();
            Self::invoke_blocking(
                engine,
                cached,
                context_owned,
                host,
                limits_owned,
                streams,
            )
        })
        .await
        .map_err(|e| WasmError::Invocation(format!("blocking wasm task failed: {e}")))?
    }

    fn invoke_blocking(
        engine: Engine,
        cached: Arc<CachedModule>,
        context: WasmInvocationContext,
        host: Arc<dyn WasmHost>,
        limits: WasmResourceLimits,
        streams: Arc<RwLock<StreamRegistry>>,
    ) -> Result<WasmInvocationResult, WasmError> {
        let start = std::time::Instant::now(); // determinism-ok: wall-clock timing for WASM sandbox
        let context_json = serde_json::to_string(&context)
            .map_err(|e| WasmError::Invocation(format!("failed to serialize context: {e}")))?;

        let needs_wasi = cached
            .module
            .imports()
            .any(|imp| imp.module() == "wasi_snapshot_preview1");
        telemetry::record_invocation_start(&context, needs_wasi, &streams);

        let wasi_stderr_pipe = if needs_wasi {
            Some(wasmtime_wasi::pipe::MemoryOutputPipe::new(64 * 1024))
        } else {
            None
        };
        let wasi_ctx = if needs_wasi {
            let mut builder = WasiCtxBuilder::new();
            if let Some(ref pipe) = wasi_stderr_pipe {
                builder.stderr(pipe.clone());
            }
            Some(builder.build_p1())
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
        let mut store = Store::new(&engine, host_state);
        store
            .set_fuel(limits.max_fuel)
            .map_err(|e| WasmError::Invocation(format!("failed to set fuel: {e}")))?;
        store.limiter(|state| &mut state.limiter);
        store.set_epoch_deadline(1);

        let engine_for_timeout = engine.clone();
        let max_duration = limits.max_duration;
        let timeout_task = tokio::spawn(async move {
            // determinism-ok: epoch timer for WASM wall-clock timeout enforcement
            tokio::time::sleep(max_duration).await;
            engine_for_timeout.increment_epoch();
        });

        struct AbortOnDrop(tokio::task::JoinHandle<()>);
        impl Drop for AbortOnDrop {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        let _timer_guard = AbortOnDrop(timeout_task);

        let instance = if needs_wasi {
            if let Some(ref pre) = cached.instance_pre_wasi {
                pre.instantiate(&mut store)
                    .map_err(|e| WasmError::Instantiation(e.to_string()))?
            } else {
                let mut linker = Linker::new(&engine);
                host_functions::link_host_functions(&mut linker)?;
                preview1::add_to_linker_sync(&mut linker, |state: &mut HostState| {
                    state.wasi_ctx.as_mut().expect("wasi_ctx must be Some")
                })
                .map_err(|e| WasmError::Compilation(format!("failed to link WASI: {e}")))?;
                linker
                    .instantiate(&mut store, &cached.module)
                    .map_err(|e| WasmError::Instantiation(e.to_string()))?
            }
        } else if let Some(ref pre) = cached.instance_pre {
            pre.instantiate(&mut store)
                .map_err(|e| WasmError::Instantiation(e.to_string()))?
        } else {
            let mut linker = Linker::new(&engine);
            host_functions::link_host_functions(&mut linker)?;
            linker
                .instantiate(&mut store, &cached.module)
                .map_err(|e| WasmError::Instantiation(e.to_string()))?
        };

        let run_fn = instance
            .get_typed_func::<(i32, i32), i32>(&mut store, "run")
            .map_err(|e| WasmError::Invocation(format!("module missing 'run' export: {e}")))?;

        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| WasmError::Invocation("module missing 'memory' export".into()))?;

        let ctx_bytes = context_json.as_bytes();
        let ctx_ptr = 1024_usize;
        memory.write(&mut store, ctx_ptr, ctx_bytes).map_err(|e| {
            WasmError::Invocation(format!("failed to write context to memory: {e}"))
        })?;

        let result_ptr = run_fn
            .call(&mut store, (ctx_ptr as i32, ctx_bytes.len() as i32))
            .map_err(|e| telemetry::map_invoke_error(e, &context, needs_wasi, max_duration, start))?;

        let duration_ms = start.elapsed().as_millis() as u64;
        tracing::Span::current().record("duration_ms", duration_ms);

        let result_json = if let Some(ref host_result) = store.data().result_json {
            host_result.clone()
        } else if result_ptr > 0 {
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
            String::new()
        };

        if result_json.is_empty() {
            if let Some(ref pipe) = wasi_stderr_pipe {
                let stderr_bytes: Vec<u8> = pipe.contents().into();
                if !stderr_bytes.is_empty() {
                    let stderr_str = String::from_utf8_lossy(&stderr_bytes);
                    tracing::warn!(
                        module = %context.trigger_action,
                        entity_type = %context.entity_type,
                        entity_id = %context.entity_id,
                        stderr = %stderr_str,
                        "WASI module returned empty result with stderr output"
                    );
                }
            }
            return Ok(telemetry::empty_result(&context, needs_wasi, duration_ms));
        }

        let parsed =
            telemetry::parse_result_json(&result_json, &context, needs_wasi, duration_ms)?;

        Ok(telemetry::finalize_result(
            &store,
            parsed,
            &context,
            needs_wasi,
            duration_ms,
        ))
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
