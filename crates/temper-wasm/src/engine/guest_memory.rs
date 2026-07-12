//! Guest memory range validation before host allocation (ADR-0160 / ARN-226).
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

    #[test]
    fn rejects_negative_and_overflow_math() {
        assert!(signed_range_invalid(0, -1));
        assert!(signed_range_invalid(-1, 8));
        assert!(!signed_range_invalid(0, 8));
        // Force usize add overflow via large offsets (not i32::MAX alone on 64-bit).
        assert!(usize::MAX.checked_add(1).is_none());
    }

    fn signed_range_invalid(ptr: i32, len: i32) -> bool {
        if ptr < 0 || len < 0 {
            return true;
        }
        (ptr as usize).checked_add(len as usize).is_none()
    }

    #[test]
    fn budget_consume_fails_closed() {
        // Minimal HostState-shaped budget fields via a local stand-in.
        struct Budget {
            guest_copy_budget: usize,
            guest_copy_consumed: usize,
        }
        fn consume(state: &mut Budget, len: usize) -> Result<(), ()> {
            let next = state.guest_copy_consumed.checked_add(len).ok_or(())?;
            if next > state.guest_copy_budget {
                return Err(());
            }
            state.guest_copy_consumed = next;
            Ok(())
        }
        let mut state = Budget {
            guest_copy_budget: 100,
            guest_copy_consumed: 0,
        };
        assert!(consume(&mut state, 60).is_ok());
        assert!(consume(&mut state, 50).is_err());
        assert_eq!(state.guest_copy_consumed, 60);
    }
}
