//! WASM engine: compile, cache, and invoke modules.
//!
//! Modules are compiled once and cached by SHA-256 hash. Each invocation
//! gets a fresh `Store` with fuel + memory limits (TigerStyle budgets).

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use sha2::{Digest, Sha256};
use wasmtime::{Caller, Config, Engine, Linker, Module, Store};

use crate::host_trait::WasmHost;
use crate::types::{WasmInvocationContext, WasmInvocationResult, WasmResourceLimits, MAX_MODULE_SIZE};

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
    /// Requested module hash not found in cache.
    #[error("module not found: {0}")]
    ModuleNotFound(String),
}

/// Compiled module cache entry.
struct CachedModule {
    /// The compiled wasmtime module.
    module: Module,
}

/// Host state passed into the WASM store.
struct HostState {
    /// Serialized invocation context JSON.
    context_json: String,
    /// Result JSON set by the guest via host_set_result.
    result_json: Option<String>,
    /// Host capabilities (HTTP, secrets, logging).
    host: Arc<dyn WasmHost>,
    /// Resource limits for this invocation (unused field kept for future memory limiting).
    #[allow(dead_code)]
    limits: WasmResourceLimits,
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
    /// Create a new WASM engine with fuel metering enabled.
    pub fn new() -> Result<Self, WasmError> {
        let mut config = Config::new();
        config.consume_fuel(true);
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
    ) -> Result<WasmInvocationResult, WasmError> {
        let cached = {
            let cache = self.cache.read().expect("cache lock poisoned");
            cache
                .get(module_hash)
                .cloned()
                .ok_or_else(|| WasmError::ModuleNotFound(module_hash.to_string()))?
        };

        let start = std::time::Instant::now();
        let context_json = serde_json::to_string(context)
            .map_err(|e| WasmError::Invocation(format!("failed to serialize context: {e}")))?;

        // Create a fresh store with fuel budget
        let host_state = HostState {
            context_json: context_json.clone(),
            result_json: None,
            host,
            limits: limits.clone(),
        };
        let mut store = Store::new(&self.engine, host_state);
        store.set_fuel(limits.max_fuel).map_err(|e| {
            WasmError::Invocation(format!("failed to set fuel: {e}"))
        })?;

        // Link host functions
        let mut linker = Linker::new(&self.engine);
        link_host_functions(&mut linker)?;

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
        memory
            .write(&mut store, ctx_ptr, ctx_bytes)
            .map_err(|e| WasmError::Invocation(format!("failed to write context to memory: {e}")))?;

        // Call run(ptr, len) -> result_ptr
        let result_ptr = run_fn
            .call(&mut store, (ctx_ptr as i32, ctx_bytes.len() as i32))
            .map_err(|e| {
                if e.to_string().contains("fuel") {
                    WasmError::FuelExhausted
                } else {
                    WasmError::Invocation(e.to_string())
                }
            })?;

        let duration_ms = start.elapsed().as_millis() as u64;

        // Read result from module memory
        // Convention: result is stored at result_ptr, length at result_ptr-4
        let result_json = if result_ptr > 0 {
            // Read length (4 bytes before result_ptr)
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
            // Check host state for result
            store.data().result_json.clone().unwrap_or_default()
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

        let parsed: serde_json::Value = serde_json::from_str(&result_json).map_err(|e| {
            WasmError::Invocation(format!("failed to parse result JSON: {e}"))
        })?;

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

/// Link host functions into the WASM linker.
fn link_host_functions(linker: &mut Linker<HostState>) -> Result<(), WasmError> {
    // host_log(level_ptr, level_len, msg_ptr, msg_len)
    linker
        .func_wrap(
            "env",
            "host_log",
            |mut caller: Caller<'_, HostState>,
             level_ptr: i32,
             level_len: i32,
             msg_ptr: i32,
             msg_len: i32| {
                let memory = caller.get_export("memory").and_then(|e| e.into_memory());
                if let Some(memory) = memory {
                    let mut level_buf = vec![0u8; level_len as usize];
                    let mut msg_buf = vec![0u8; msg_len as usize];
                    let _ = memory.read(&caller, level_ptr as usize, &mut level_buf);
                    let _ = memory.read(&caller, msg_ptr as usize, &mut msg_buf);
                    let level = String::from_utf8_lossy(&level_buf);
                    let msg = String::from_utf8_lossy(&msg_buf);
                    caller.data().host.log(&level, &msg);
                }
            },
        )
        .map_err(|e| WasmError::Compilation(format!("failed to link host_log: {e}")))?;

    // host_get_context(buf_ptr, buf_len) -> actual_len
    linker
        .func_wrap(
            "env",
            "host_get_context",
            |mut caller: Caller<'_, HostState>, buf_ptr: i32, buf_len: i32| -> i32 {
                let ctx_json = caller.data().context_json.clone();
                let ctx_bytes = ctx_json.as_bytes();
                if (buf_len as usize) < ctx_bytes.len() {
                    return ctx_bytes.len() as i32; // Return needed size
                }
                let memory = caller.get_export("memory").and_then(|e| e.into_memory());
                if let Some(memory) = memory {
                    let _ = memory.write(&mut caller, buf_ptr as usize, ctx_bytes);
                }
                ctx_bytes.len() as i32
            },
        )
        .map_err(|e| WasmError::Compilation(format!("failed to link host_get_context: {e}")))?;

    // host_set_result(ptr, len)
    linker
        .func_wrap(
            "env",
            "host_set_result",
            |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| {
                let memory = caller.get_export("memory").and_then(|e| e.into_memory());
                if let Some(memory) = memory {
                    let mut buf = vec![0u8; len as usize];
                    let _ = memory.read(&caller, ptr as usize, &mut buf);
                    if let Ok(s) = String::from_utf8(buf) {
                        caller.data_mut().result_json = Some(s);
                    }
                }
            },
        )
        .map_err(|e| WasmError::Compilation(format!("failed to link host_set_result: {e}")))?;

    // host_get_secret(key_ptr, key_len, buf_ptr, buf_len) -> actual_len (-1 on error)
    linker
        .func_wrap(
            "env",
            "host_get_secret",
            |mut caller: Caller<'_, HostState>,
             key_ptr: i32,
             key_len: i32,
             buf_ptr: i32,
             buf_len: i32|
             -> i32 {
                let memory = caller.get_export("memory").and_then(|e| e.into_memory());
                let Some(memory) = memory else { return -1 };

                let mut key_buf = vec![0u8; key_len as usize];
                let _ = memory.read(&caller, key_ptr as usize, &mut key_buf);
                let key = String::from_utf8_lossy(&key_buf);

                match caller.data().host.get_secret(&key) {
                    Ok(secret) => {
                        let secret_bytes = secret.as_bytes();
                        if (buf_len as usize) < secret_bytes.len() {
                            return secret_bytes.len() as i32;
                        }
                        let _ = memory.write(&mut caller, buf_ptr as usize, secret_bytes);
                        secret_bytes.len() as i32
                    }
                    Err(_) => -1,
                }
            },
        )
        .map_err(|e| WasmError::Compilation(format!("failed to link host_get_secret: {e}")))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_module_deterministic() {
        let bytes = b"test wasm bytes";
        let h1 = WasmEngine::hash_module(bytes);
        let h2 = WasmEngine::hash_module(bytes);
        assert_eq!(h1, h2);
        assert!(!h1.is_empty());
    }

    #[test]
    fn engine_creation() {
        let engine = WasmEngine::new();
        assert!(engine.is_ok());
    }

    #[test]
    fn module_too_large_rejected() {
        let engine = WasmEngine::new().unwrap();
        let big = vec![0u8; MAX_MODULE_SIZE + 1];
        let result = engine.compile_and_cache(&big);
        assert!(matches!(result, Err(WasmError::ModuleTooLarge { .. })));
    }

    #[test]
    fn resource_limits_default() {
        let limits = WasmResourceLimits::default();
        assert_eq!(limits.max_fuel, 1_000_000_000);
        assert_eq!(limits.max_memory, 16 * 1024 * 1024);
        assert_eq!(limits.max_duration, std::time::Duration::from_secs(30));
        assert_eq!(limits.max_response_bytes, 1024 * 1024);
    }
}
