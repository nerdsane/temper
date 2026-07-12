//! Guest memory range validation before host allocation (ADR-0163 / ARN-226).
//!
//! All guest→host copies must prove signedness, range, and remaining budget
//! **before** allocating a host buffer.

use wasmtime::{AsContext, Caller, Memory};

use super::HostState;

/// Default per-call max when limits are not tighter (matches 1 MiB response default).
pub const DEFAULT_MAX_GUEST_COPY: usize = 1024 * 1024;

/// Validated guest range ready for a bounded host allocation.
#[derive(Debug, Clone, Copy)]
pub struct GuestRange {
    /// Byte offset into linear memory.
    pub offset: usize,
    /// Byte length.
    pub len: usize,
}

/// Validate ptr/len and linear-memory containment without allocating.
pub fn validate_guest_range(
    memory: &Memory,
    store: impl wasmtime::AsContext,
    ptr: i32,
    len: i32,
    host_fn: &'static str,
    what: &'static str,
) -> Result<GuestRange, ()> {
    if ptr < 0 || len < 0 {
        tracing::warn!(
            host_fn,
            operand = what,
            ptr,
            len,
            "guest passed negative pointer or length; refusing allocate"
        );
        return Err(());
    }

    let offset = ptr as usize;
    let len_u = len as usize;
    // Defense in depth: checked_add on 64-bit hosts is unreachable for i32 operands
    // (max sum ~4 GiB << usize::MAX) but kept so 32-bit and future wider inputs stay safe.
    let end = offset.checked_add(len_u).ok_or_else(|| {
        tracing::warn!(
            host_fn,
            operand = what,
            ptr,
            len,
            "guest pointer+length overflows; refusing allocate"
        );
    })?;

    let mem_size = memory.data_size(store);
    if end > mem_size {
        tracing::warn!(
            host_fn,
            operand = what,
            ptr,
            len,
            mem_size,
            end,
            "guest range exceeds linear memory; refusing allocate"
        );
        return Err(());
    }

    Ok(GuestRange { offset, len: len_u })
}

/// Consume aggregate guest-copy budget for this invocation.
pub fn consume_guest_copy_budget(
    state: &mut HostState,
    len: usize,
    host_fn: &'static str,
    what: &'static str,
) -> Result<(), ()> {
    let next = state.guest_copy_consumed.checked_add(len).ok_or_else(|| {
        tracing::warn!(
            host_fn,
            operand = what,
            len,
            "guest copy budget arithmetic overflow"
        );
    })?;
    if next > state.guest_copy_budget {
        tracing::warn!(
            host_fn,
            operand = what,
            len,
            consumed = state.guest_copy_consumed,
            budget = state.guest_copy_budget,
            "guest copy budget exhausted; refusing allocate"
        );
        return Err(());
    }
    state.guest_copy_consumed = next;
    Ok(())
}

/// Read guest bytes only after range + budget validation (allocates exactly `len`).
pub fn read_guest_bytes_checked(
    caller: &mut Caller<'_, HostState>,
    memory: &Memory,
    ptr: i32,
    len: i32,
    host_fn: &'static str,
    what: &'static str,
) -> Result<Vec<u8>, ()> {
    let range = {
        let store = caller.as_context();
        validate_guest_range(memory, store, ptr, len, host_fn, what)?
    };

    {
        let data = caller.data_mut();
        consume_guest_copy_budget(data, range.len, host_fn, what)?;
    }

    let mut buf = vec![0u8; range.len];
    memory
        .read(&mut *caller, range.offset, &mut buf)
        .map_err(|error| {
            tracing::warn!(
                host_fn,
                operand = what,
                error = %error,
                "guest memory read failed after validation"
            );
        })?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};
    use std::time::{Duration, Instant};

    use wasmtime::{Engine, Memory, MemoryType, Store};

    use super::super::{GuestSpanRegistry, MemoryLimiter};
    use super::*;
    use crate::host_trait::SimWasmHost;
    use crate::stream::StreamRegistry;
    use crate::types::WasmInvocationContext;

    fn test_invocation_ctx() -> WasmInvocationContext {
        WasmInvocationContext {
            tenant: "default".into(),
            entity_type: "Test".into(),
            entity_id: "1".into(),
            trigger_action: "Run".into(),
            wasm_module: None,
            trigger_params: serde_json::Value::Null,
            entity_state: serde_json::Value::Null,
            agent_id: None,
            session_id: None,
            integration_config: Default::default(),
            trace_id: String::new(),
            workflow_root_entity_type: None,
            workflow_root_entity_id: None,
            workflow_run_id: None,
            http_request: None,
        }
    }

    fn test_host_state(budget: usize) -> HostState {
        HostState {
            context_json: "{}".into(),
            result_json: None,
            host: Arc::new(SimWasmHost::new()),
            host_call_deadline: Instant::now() + Duration::from_secs(30),
            limiter: MemoryLimiter {
                max_memory: 16 * 1024 * 1024,
            },
            streams: Arc::new(RwLock::new(StreamRegistry::new())),
            wasi_ctx: None,
            blob_cache: Default::default(),
            guest_spans: GuestSpanRegistry::new(test_invocation_ctx()),
            guest_copy_budget: budget,
            guest_copy_consumed: 0,
        }
    }

    fn memory_with_pages(pages: u32) -> (Store<()>, Memory) {
        let engine = Engine::default();
        let mut store = Store::new(&engine, ());
        let memory = Memory::new(&mut store, MemoryType::new(pages, Some(pages))).expect("memory");
        (store, memory)
    }

    #[test]
    fn validate_guest_range_rejects_negative_and_oob() {
        let (mut store, memory) = memory_with_pages(1); // 64 KiB
        assert!(validate_guest_range(&memory, &store, -1, 8, "test", "buf").is_err());
        assert!(validate_guest_range(&memory, &store, 0, -1, "test", "buf").is_err());
        assert!(validate_guest_range(&memory, &store, 0, 8, "test", "buf").is_ok());
        // Beyond linear memory (1 page = 65536).
        assert!(validate_guest_range(&memory, &store, 65_000, 1_000, "test", "buf").is_err());
        // Exactly at end is allowed when end == mem_size (0-length at end).
        let mem_size = memory.data_size(&store) as i32;
        assert!(validate_guest_range(&memory, &mut store, mem_size, 0, "test", "buf").is_ok());
        assert!(validate_guest_range(&memory, &mut store, mem_size, 1, "test", "buf").is_err());
    }

    #[test]
    fn consume_guest_copy_budget_fails_closed() {
        let mut state = test_host_state(100);
        assert!(consume_guest_copy_budget(&mut state, 60, "test", "buf").is_ok());
        assert_eq!(state.guest_copy_consumed, 60);
        assert!(consume_guest_copy_budget(&mut state, 50, "test", "buf").is_err());
        // Failed consume must not advance the counter.
        assert_eq!(state.guest_copy_consumed, 60);
        assert!(consume_guest_copy_budget(&mut state, 40, "test", "buf").is_ok());
        assert_eq!(state.guest_copy_consumed, 100);
    }
}
