//! WASM engine: compile, cache, and invoke modules.
//!
//! Modules are compiled once and cached by SHA-256 hash. Each invocation
//! gets a fresh `Store` with fuel + memory limits (TigerStyle budgets).

mod data_host_functions;
#[cfg(test)]
#[path = "guest_read_bounds_test.rs"]
mod guest_read_bounds_test;
mod guest_spans;
mod host_functions;
mod migration;
mod telemetry;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use wasmtime::{Config, Engine, Linker, Module, ProfilingStrategy, ResourceLimiter, Store};
use wasmtime_wasi::preview1::WasiP1Ctx;
use wasmtime_wasi::{WasiCtxBuilder, preview1};

use crate::host_trait::WasmHost;
use crate::stream::StreamRegistry;
use crate::types::{
    MAX_MODULE_SIZE, WasmInvocationContext, WasmInvocationResult, WasmResourceLimits,
};

pub use migration::{PureMigrationError, PureMigrationLimits};

pub(crate) use guest_spans::GuestSpanRegistry;

const WASM_EPOCH_TICK_INTERVAL: Duration = Duration::from_millis(10);
const WASM_INVOKE_THREAD_STACK_BYTES: usize = 16 * 1024 * 1024;

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
        desired: usize,
        _maximum: Option<usize>,
    ) -> Result<bool, wasmtime::Error> {
        // Bound table growth too: table elements are host allocations (a funcref/
        // externref slot each) that the memory limiter does not cover, so an
        // unbounded `table.grow` is a host-memory exhaustion vector (ARN-169).
        // Deny past the cap (grow returns -1) rather than trapping.
        Ok(desired <= MAX_TABLE_ELEMENTS)
    }

    // The per-table/per-memory element caps above only bound host allocation if
    // the *number* of tables and memories is also bounded — wasmtime's default
    // limiter allows 10,000 of each per store, so a guest could otherwise declare
    // thousands of tables each at the element cap and multiply the budget. Cap the
    // counts to what a single-module guest actually needs (ARN-169).
    fn memories(&self) -> usize {
        MAX_MEMORIES
    }

    fn tables(&self) -> usize {
        MAX_TABLES
    }
}

/// Maximum number of elements any single guest table may hold. Table slots are
/// host allocations outside the linear-memory budget; this caps the host memory a
/// guest can force through `table.grow` (ARN-169).
const MAX_TABLE_ELEMENTS: usize = 1_000_000;

/// Maximum number of linear memories a guest store may create. With
/// `wasm_multi_memory(false)` a module already declares at most one; this makes
/// the store-level bound explicit so the per-memory `max_memory` budget cannot be
/// multiplied (ARN-169).
const MAX_MEMORIES: usize = 1;

/// Maximum number of tables a guest store may create. Together with
/// `MAX_TABLE_ELEMENTS` this bounds total table host memory to
/// `MAX_TABLES * MAX_TABLE_ELEMENTS` slots. Generous enough for ordinary
/// reference-types modules (typically one funcref table) while preventing the
/// default 10,000-table multiplier (ARN-169).
const MAX_TABLES: usize = 8;

/// Compiled module cache entry.
struct CachedModule {
    /// SHA-256 hash that keys this compiled module in the cache.
    hash: String,
    /// The compiled wasmtime module.
    module: Module,
    /// Pre-linked instance template for non-WASI modules.
    instance_pre: Option<wasmtime::InstancePre<HostState>>,
    /// Pre-linked instance template for WASI modules.
    instance_pre_wasi: Option<wasmtime::InstancePre<HostState>>,
}

/// Background ticker for Wasmtime epoch interruption.
///
/// Wasmtime epochs are global to an `Engine`; per-invocation timeout tasks that
/// call `increment_epoch()` can therefore interrupt every active store sharing
/// that engine. A single monotonic ticker keeps epoch increments global while
/// each store's deadline remains local and relative to the epoch at store setup.
struct EpochTicker {
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl EpochTicker {
    fn start(engine: Engine) -> Result<Self, WasmError> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop);
        let handle = thread::Builder::new()
            .name("temper-wasm-epoch".to_string())
            .spawn(move || {
                while !stop_for_thread.load(Ordering::Acquire) {
                    thread::sleep(WASM_EPOCH_TICK_INTERVAL);
                    if stop_for_thread.load(Ordering::Acquire) {
                        break;
                    }
                    engine.increment_epoch();
                }
            })
            .map_err(|e| WasmError::Compilation(format!("failed to start epoch ticker: {e}")))?;

        Ok(Self {
            stop,
            handle: Some(handle),
        })
    }
}

impl Drop for EpochTicker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn epoch_deadline_ticks(max_duration: Duration) -> u64 {
    let tick_nanos = WASM_EPOCH_TICK_INTERVAL.as_nanos().max(1);
    let duration_nanos = max_duration.as_nanos();
    let ticks = duration_nanos.saturating_add(tick_nanos.saturating_sub(1)) / tick_nanos;
    u64::try_from(ticks.max(1)).unwrap_or(u64::MAX)
}

/// Host state passed into the WASM store.
pub(crate) struct HostState {
    /// Serialized invocation context JSON.
    pub(crate) context_json: String,
    /// Result JSON set by the guest via host_set_result.
    pub(crate) result_json: Option<String>,
    /// Host capabilities (HTTP, secrets, logging).
    pub(crate) host: Arc<dyn WasmHost>,
    /// Wall-clock deadline shared by async host calls for this invocation.
    pub(crate) host_call_deadline: Instant,
    /// Memory limiter enforcing `max_memory` per invocation.
    limiter: MemoryLimiter,
    /// Stream registry for binary data transfer between host and WASM guest.
    /// Bytes never enter WASM memory — WASM references them by stream ID.
    pub(crate) streams: Arc<RwLock<StreamRegistry>>,
    /// WASI context for modules compiled with wasm32-wasi target.
    /// None for wasm32-unknown-unknown modules. When present, WASI
    /// syscalls (clock_time_get, random_get, etc.) are available.
    pub(crate) wasi_ctx: Option<WasiP1Ctx>,
    /// Pre-fetched bytes for oversize blob-ref fields referenced by
    /// `context_json`. Populated by the dispatcher before the invocation
    /// enters the blocking WASM thread, so `host_read_field_stream` can resolve
    /// blob refs synchronously. Keyed by blob key (e.g.
    /// `field-overflow/sha256/<hex>.json`). See ADR-0046.
    pub(crate) blob_cache: BTreeMap<String, Vec<u8>>,
    /// Guest-created observability spans scoped to this invocation.
    pub(crate) guest_spans: GuestSpanRegistry,
    /// Bounded response byte buffers keyed by invocation-local handle.
    pub(crate) data_responses: BTreeMap<i32, Vec<u8>>,
    /// Next positive response handle.
    pub(crate) next_data_response: i32,
}

impl HostState {
    /// Remaining wall-clock budget for an async host call.
    pub(crate) fn remaining_host_call_timeout(&self) -> Duration {
        let remaining = self
            .host_call_deadline
            .saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            Duration::from_millis(1)
        } else {
            remaining
        }
    }
}

/// WASM engine: compile, cache, invoke modules.
///
/// Modules are compiled once and cached by SHA-256 hash. Each invocation
/// gets a fresh `Store` with fuel + memory limits (TigerStyle budgets).
pub struct WasmEngine {
    /// The underlying wasmtime engine.
    engine: Engine,
    /// Shared epoch ticker used by per-store relative deadlines.
    _epoch_ticker: EpochTicker,
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
        // Pin the WASM feature surface across the wasmtime 29 -> 36 bump (ARN-169).
        // wasmtime 30+ turns several proposals on by default; leaving them at the
        // 36 defaults would silently widen the guest attack surface relative to 29:
        //   - memory64: rejected at compile on 29, and a prerequisite for the very
        //     advisory this bump fixes (RUSTSEC-2026-0096) — keep it off;
        //   - threads/shared memory and multiple memories: each lets a guest hold a
        //     linear memory the per-memory `max_memory` limiter does not sum, so a
        //     guest could exceed the intended per-invocation memory budget.
        // Temper's guests are single-memory wasm32 modules and use none of these.
        config.wasm_memory64(false);
        config.wasm_threads(false);
        config.wasm_multi_memory(false);
        if let Some(strategy) = configured_profiling_strategy() {
            config.profiler(strategy);
        }

        let engine = Engine::new(&config).map_err(|e| WasmError::Compilation(e.to_string()))?;
        let epoch_ticker = EpochTicker::start(engine.clone())?;

        Ok(Self {
            engine,
            _epoch_ticker: epoch_ticker,
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
            hash: hash.clone(),
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
    pub async fn invoke(
        &self,
        module_hash: &str,
        context: &WasmInvocationContext,
        host: Arc<dyn WasmHost>,
        limits: &WasmResourceLimits,
        streams: Arc<RwLock<StreamRegistry>>,
    ) -> Result<WasmInvocationResult, WasmError> {
        self.invoke_with_blobs(module_hash, context, host, limits, streams, BTreeMap::new())
            .await
    }

    /// Invoke a cached WASM module with pre-fetched blob-ref bytes available to
    /// the guest via `host_read_field_stream`. See ADR-0046.
    ///
    /// `blob_cache` maps blob keys (e.g. `field-overflow/sha256/<hex>.json`) to
    /// their decoded bytes. Keys correspond to oversize blob refs present in
    /// `context.entity_state` that exceed the caller's declared inline ceiling.
    /// Keys the guest does not read are discarded with the invocation.
    #[tracing::instrument(
        name = "wasm.invoke",
        skip(self, context, host, limits, streams, blob_cache),
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
            blob_cache_entries = tracing::field::Empty,
            blob_cache_bytes = tracing::field::Empty,
            context_bytes = tracing::field::Empty,
            needs_wasi = tracing::field::Empty,
            success = tracing::field::Empty,
            callback_action = tracing::field::Empty,
            duration_ms = tracing::field::Empty,
            error = tracing::field::Empty,
            error.type = tracing::field::Empty,
            error.message = tracing::field::Empty,
            exception.message = tracing::field::Empty,
        )
    )]
    pub async fn invoke_with_blobs(
        &self,
        module_hash: &str,
        context: &WasmInvocationContext,
        host: Arc<dyn WasmHost>,
        limits: &WasmResourceLimits,
        streams: Arc<RwLock<StreamRegistry>>,
        blob_cache: BTreeMap<String, Vec<u8>>,
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
        let blob_cache_entries = blob_cache.len() as u64;
        let blob_cache_bytes = blob_cache
            .values()
            .map(|bytes| bytes.len() as u64)
            .sum::<u64>();
        tracing::Span::current().record("blob_cache_entries", blob_cache_entries);
        tracing::Span::current().record("blob_cache_bytes", blob_cache_bytes);
        let span = tracing::Span::current();
        let runtime_handle = tokio::runtime::Handle::current();

        let (tx, rx) = tokio::sync::oneshot::channel();
        let thread_name = format!(
            "temper-wasm-invoke-{}",
            module_hash.chars().take(12).collect::<String>()
        );

        thread::Builder::new()
            .name(thread_name)
            .stack_size(WASM_INVOKE_THREAD_STACK_BYTES)
            .spawn(move || {
                let result = {
                    let _runtime_entered = runtime_handle.enter();
                    let _entered = span.enter();
                    Self::invoke_blocking(
                        engine,
                        cached,
                        context_owned,
                        host,
                        limits_owned,
                        streams,
                        blob_cache,
                    )
                };
                let _ = tx.send(result);
            })
            .map_err(|e| WasmError::Invocation(format!("failed to spawn WASM thread: {e}")))?;

        rx.await
            .map_err(|e| WasmError::Invocation(format!("WASM thread terminated: {e}")))?
    }

    fn invoke_blocking(
        engine: Engine,
        cached: Arc<CachedModule>,
        context: WasmInvocationContext,
        host: Arc<dyn WasmHost>,
        limits: WasmResourceLimits,
        streams: Arc<RwLock<StreamRegistry>>,
        blob_cache: BTreeMap<String, Vec<u8>>,
    ) -> Result<WasmInvocationResult, WasmError> {
        let module_hash = cached.hash.as_str();
        let invocation_span = tracing::Span::current();
        let start = std::time::Instant::now(); // determinism-ok: wall-clock timing for WASM sandbox
        let context_json = {
            let phase = tracing::info_span!(
                "wasm.invoke.serialize_context",
                otel.name = "wasm.invoke.serialize_context",
                module_hash = %module_hash,
                context_bytes = tracing::field::Empty,
            );
            let _entered = phase.enter();
            let serialized = serde_json::to_string(&context)
                .map_err(|e| WasmError::Invocation(format!("failed to serialize context: {e}")))?;
            let context_bytes = serialized.len() as u64;
            phase.record("context_bytes", context_bytes);
            invocation_span.record("context_bytes", context_bytes);
            serialized
        };

        let needs_wasi = cached
            .module
            .imports()
            .any(|imp| imp.module() == "wasi_snapshot_preview1");
        telemetry::record_invocation_start(&context, needs_wasi, &streams);

        let (wasi_stderr_pipe, host_state) = {
            let phase = tracing::info_span!(
                "wasm.invoke.prepare_host_state",
                otel.name = "wasm.invoke.prepare_host_state",
                module_hash = %module_hash,
                needs_wasi,
                blob_cache_entries = blob_cache.len() as u64,
            );
            let _entered = phase.enter();
            let wasi_stderr_pipe = if needs_wasi {
                Some(wasmtime_wasi::p2::pipe::MemoryOutputPipe::new(64 * 1024))
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

            // ADR-0166: the guest-facing span API is fed by an untrusted module, so
            // it needs the same per-tenant content decision the host HTTP path uses.
            // Read it from the host itself, whose default is redact.
            let export_llm_content = host.exports_llm_content();
            let host_state = HostState {
                context_json: context_json.clone(),
                result_json: None,
                host,
                host_call_deadline: start.checked_add(limits.max_duration).unwrap_or(start),
                limiter: MemoryLimiter {
                    max_memory: limits.max_memory,
                },
                streams,
                wasi_ctx,
                blob_cache,
                guest_spans: GuestSpanRegistry::for_invocation(
                    context.clone(),
                    needs_wasi,
                    export_llm_content,
                ),
                data_responses: BTreeMap::new(),
                next_data_response: 1,
            };
            (wasi_stderr_pipe, host_state)
        };

        let mut store = {
            let phase = tracing::info_span!(
                "wasm.invoke.create_store",
                otel.name = "wasm.invoke.create_store",
                module_hash = %module_hash,
                needs_wasi,
                max_fuel = limits.max_fuel,
                max_memory = limits.max_memory as u64,
            );
            let _entered = phase.enter();
            let mut store = Store::new(&engine, host_state);
            store
                .set_fuel(limits.max_fuel)
                .map_err(|e| WasmError::Invocation(format!("failed to set fuel: {e}")))?;
            store.limiter(|state| &mut state.limiter);
            store.set_epoch_deadline(epoch_deadline_ticks(limits.max_duration));
            store
        };

        let instance = {
            let phase = tracing::info_span!(
                "wasm.invoke.instantiate",
                otel.name = "wasm.invoke.instantiate",
                module_hash = %module_hash,
                needs_wasi,
                prelinked = true,
            );
            let _entered = phase.enter();
            if needs_wasi {
                if let Some(ref pre) = cached.instance_pre_wasi {
                    pre.instantiate(&mut store)
                        .map_err(|e| WasmError::Instantiation(e.to_string()))?
                } else {
                    phase.record("prelinked", false);
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
                phase.record("prelinked", false);
                let mut linker = Linker::new(&engine);
                host_functions::link_host_functions(&mut linker)?;
                linker
                    .instantiate(&mut store, &cached.module)
                    .map_err(|e| WasmError::Instantiation(e.to_string()))?
            }
        };

        let (run_fn, memory) = {
            let phase = tracing::info_span!(
                "wasm.invoke.bind_exports",
                otel.name = "wasm.invoke.bind_exports",
                module_hash = %module_hash,
            );
            let _entered = phase.enter();
            let run_fn = instance
                .get_typed_func::<(i32, i32), i32>(&mut store, "run")
                .map_err(|e| WasmError::Invocation(format!("module missing 'run' export: {e}")))?;

            let memory = instance
                .get_memory(&mut store, "memory")
                .ok_or_else(|| WasmError::Invocation("module missing 'memory' export".into()))?;
            (run_fn, memory)
        };

        let ctx_bytes = context_json.as_bytes();
        let mut ctx_ptr = 1024_usize;
        {
            let phase = tracing::info_span!(
                "wasm.invoke.write_context",
                otel.name = "wasm.invoke.write_context",
                module_hash = %module_hash,
                context_bytes = ctx_bytes.len() as u64,
            );
            let _entered = phase.enter();
            let current_len = memory.data_size(&store);
            if ctx_bytes.len() <= current_len.saturating_sub(ctx_ptr) {
                memory.write(&mut store, ctx_ptr, ctx_bytes).map_err(|e| {
                    WasmError::Invocation(format!("failed to write context to memory: {e}"))
                })?;
            } else {
                // SDK-backed modules read the canonical context through host_get_context.
                // Avoid growing and dirtying fresh guest heap pages before the guest
                // allocator can initialize them; that can corrupt large-context parses.
                ctx_ptr = 0;
            }
        }

        let result_ptr = {
            let phase = tracing::info_span!(
                "wasm.invoke.run",
                otel.name = "wasm.invoke.run",
                module_hash = %module_hash,
                context_bytes = ctx_bytes.len() as u64,
            );
            let _entered = phase.enter();
            match run_fn.call(&mut store, (ctx_ptr as i32, ctx_bytes.len() as i32)) {
                Ok(result_ptr) => result_ptr,
                Err(e) => {
                    let err = telemetry::map_invoke_error(
                        e,
                        &context,
                        needs_wasi,
                        limits.max_duration,
                        start,
                    );
                    store.data_mut().guest_spans.cleanup_unclosed();
                    return Err(err);
                }
            }
        };

        let duration_ms = start.elapsed().as_millis() as u64;
        tracing::Span::current().record("duration_ms", duration_ms);

        let result_json = {
            let phase = tracing::info_span!(
                "wasm.invoke.read_result",
                otel.name = "wasm.invoke.read_result",
                module_hash = %module_hash,
                result_source = tracing::field::Empty,
                result_bytes = tracing::field::Empty,
            );
            let _entered = phase.enter();
            if let Some(ref host_result) = store.data().result_json {
                phase.record("result_source", "host_set_result");
                phase.record("result_bytes", host_result.len() as u64);
                host_result.clone()
            } else if result_ptr > 0 {
                phase.record("result_source", "memory_ptr");
                let mut len_bytes = [0u8; 4];
                if let Err(e) = memory.read(&store, (result_ptr - 4) as usize, &mut len_bytes) {
                    store.data_mut().guest_spans.cleanup_unclosed();
                    return Err(WasmError::Invocation(format!(
                        "failed to read result length: {e}"
                    )));
                }
                let result_len = u32::from_le_bytes(len_bytes) as usize;
                phase.record("result_bytes", result_len as u64);

                // ARN-226: bound the result allocation by the guest's memory size
                // before allocating, so a forged length prefix can't drive a large
                // host allocation ahead of the bounds check.
                if !host_functions::guest_read_bounds_ok(
                    memory.data_size(&store),
                    result_ptr as usize,
                    result_len,
                ) {
                    store.data_mut().guest_spans.cleanup_unclosed();
                    return Err(WasmError::Invocation(
                        "result length exceeds guest linear memory".to_string(),
                    ));
                }
                let mut result_bytes = vec![0u8; result_len];
                if let Err(e) = memory.read(&store, result_ptr as usize, &mut result_bytes) {
                    store.data_mut().guest_spans.cleanup_unclosed();
                    return Err(WasmError::Invocation(format!("failed to read result: {e}")));
                }

                match String::from_utf8(result_bytes) {
                    Ok(result) => result,
                    Err(e) => {
                        store.data_mut().guest_spans.cleanup_unclosed();
                        return Err(WasmError::Invocation(format!(
                            "result is not valid UTF-8: {e}"
                        )));
                    }
                }
            } else {
                phase.record("result_source", "empty");
                phase.record("result_bytes", 0_u64);
                String::new()
            }
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
            let result = telemetry::empty_result(&context, needs_wasi, duration_ms);
            store.data_mut().guest_spans.cleanup_unclosed();
            return Ok(result);
        }

        let parsed = {
            let phase = tracing::info_span!(
                "wasm.invoke.parse_result",
                otel.name = "wasm.invoke.parse_result",
                module_hash = %module_hash,
                result_bytes = result_json.len() as u64,
            );
            let _entered = phase.enter();
            match telemetry::parse_result_json(&result_json, &context, needs_wasi, duration_ms) {
                Ok(parsed) => parsed,
                Err(err) => {
                    store.data_mut().guest_spans.cleanup_unclosed();
                    return Err(err);
                }
            }
        };

        let result = telemetry::finalize_result(&store, parsed, &context, needs_wasi, duration_ms);
        store.data_mut().guest_spans.cleanup_unclosed();
        Ok(result)
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

fn configured_profiling_strategy() -> Option<ProfilingStrategy> {
    match std::env::var("TEMPER_WASM_PROFILING") {
        Ok(value) => match value.trim().to_ascii_lowercase().as_str() {
            "" | "0" | "false" | "off" | "none" => None,
            "1" | "true" | "on" | "perf" | "perfmap" | "perf-map" => {
                Some(ProfilingStrategy::PerfMap)
            }
            "jitdump" | "jit-dump" => Some(ProfilingStrategy::JitDump),
            "vtune" => Some(ProfilingStrategy::VTune),
            other => {
                tracing::warn!(
                    temper_wasm_profiling = other,
                    "unknown TEMPER_WASM_PROFILING value; WASM guest profiling disabled"
                );
                None
            }
        },
        Err(_) => None,
    }
}

impl Default for WasmEngine {
    fn default() -> Self {
        Self::new().expect("failed to create default WasmEngine")
    }
}
