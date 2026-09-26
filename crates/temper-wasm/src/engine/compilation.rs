//! Compile immutable code separately from an engine handle's registrations.
//!
//! Tests can share the compiler across simulated restarts without making an
//! unregistered module invocable or retaining any guest Store/host capability.
//! Normal production builds keep a private engine and the cold compilation path.

use super::*;
use wasmtime::Linker;
use wasmtime_wasi::preview1;

impl WasmEngine {
    /// Compile/pre-link code, consulting the test cache when enabled.
    pub(super) fn compile_module(
        &self,
        hash: &str,
        wasm_bytes: &[u8],
    ) -> Result<Arc<CachedModule>, WasmError> {
        #[cfg(any(test, feature = "test-shared-compilation"))]
        if let Some(compiler) = &self.shared_compiler {
            // Single-flight: simultaneous test boots must not compile the same
            // module independently. Compilation/pre-linking runs no guest code.
            let mut modules = compiler.modules.lock().expect("compiler cache poisoned");
            if let Some(module) = modules.get(hash) {
                return Ok(Arc::clone(module));
            }
            let module = self.compile_uncached(hash, wasm_bytes)?;
            if modules.len() >= MAX_SHARED_MODULES {
                // Deterministic eviction. Active registrations retain their Arc,
                // so evicting reusable code cannot invalidate a running engine.
                modules.pop_first();
            }
            modules.insert(hash.to_owned(), Arc::clone(&module));
            return Ok(module);
        }
        self.compile_uncached(hash, wasm_bytes)
    }

    fn compile_uncached(
        &self,
        hash: &str,
        wasm_bytes: &[u8],
    ) -> Result<Arc<CachedModule>, WasmError> {
        let module = Module::new(&self.engine, wasm_bytes)
            .map_err(|e| WasmError::Compilation(e.to_string()))?;
        let needs_wasi = module
            .imports()
            .any(|imp| imp.module() == "wasi_snapshot_preview1");
        let mut linker = Linker::new(&self.engine);
        host_functions::link_host_functions(&mut linker)
            .map_err(|e| WasmError::Compilation(format!("pre-link host functions: {e}")))?;
        let (instance_pre, instance_pre_wasi) = if needs_wasi {
            preview1::add_to_linker_sync(&mut linker, |state: &mut HostState| {
                state.wasi_ctx.as_mut().expect("wasi_ctx must be Some")
            })
            .map_err(|e| WasmError::Compilation(format!("pre-link WASI: {e}")))?;
            let pre = linker
                .instantiate_pre(&module)
                .map_err(|e| WasmError::Compilation(format!("pre-instantiate WASI: {e}")))?;
            (None, Some(pre))
        } else {
            let pre = linker
                .instantiate_pre(&module)
                .map_err(|e| WasmError::Compilation(format!("pre-instantiate: {e}")))?;
            (Some(pre), None)
        };
        tracing::info!(%hash, size = wasm_bytes.len(), "WASM module compiled and cached");
        Ok(Arc::new(CachedModule {
            hash: hash.to_owned(),
            module,
            instance_pre,
            instance_pre_wasi,
        }))
    }
}

#[cfg(any(test, feature = "test-shared-compilation"))]
use std::sync::{Mutex, OnceLock};

/// Bound retained code independently of the number of simulated restarts.
#[cfg(any(test, feature = "test-shared-compilation"))]
pub(super) const MAX_SHARED_MODULES: usize = 128;

#[cfg(any(test, feature = "test-shared-compilation"))]
/// Shared immutable code and its compatible Wasmtime engine/ticker.
pub(super) struct SharedCompiler {
    engine: Engine,
    epoch_ticker: Arc<EpochTicker>,
    pub(super) modules: Mutex<BTreeMap<String, Arc<CachedModule>>>,
}

#[cfg(any(test, feature = "test-shared-compilation"))]
impl SharedCompiler {
    /// Create a private compiler using the production engine configuration.
    pub(super) fn new() -> Result<Self, WasmError> {
        let private = WasmEngine::new_private()?;
        Ok(Self {
            engine: private.engine,
            epoch_ticker: private._epoch_ticker,
            modules: Mutex::new(BTreeMap::new()),
        })
    }

    /// Create a fresh registration scope backed by this compiler.
    pub(super) fn new_engine(self: &Arc<Self>) -> WasmEngine {
        WasmEngine {
            engine: self.engine.clone(),
            _epoch_ticker: Arc::clone(&self.epoch_ticker),
            cache: RwLock::new(BTreeMap::new()),
            http_clients: HttpClientCache::default(),
            shared_compiler: Some(Arc::clone(self)),
        }
    }
}

#[cfg(any(test, feature = "test-shared-compilation"))]
/// Create a fresh registration scope backed by the process-local test compiler.
pub(super) fn shared_engine() -> Result<WasmEngine, WasmError> {
    // Only immutable code is process-global. Registrations, tenants, secrets,
    // linear memory, fuel, host state, and deadlines remain per handle/invocation.
    // Profiling-enabled engines bypass this cache in WasmEngine::new().
    static COMPILER: OnceLock<Result<Arc<SharedCompiler>, String>> = OnceLock::new();
    match COMPILER.get_or_init(|| {
        SharedCompiler::new()
            .map(Arc::new)
            .map_err(|err| err.to_string())
    }) {
        Ok(compiler) => Ok(compiler.new_engine()),
        Err(err) => Err(WasmError::Compilation(err.clone())),
    }
}
